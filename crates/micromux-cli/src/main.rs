//! `micromux-cli` is the command-line entry point for micromux.
//!
//! It is responsible for:
//! - Locating and parsing the configuration file.
//! - Emitting diagnostics.
//! - Starting the TUI and scheduler.

mod attach;
mod control;
mod ctl;
mod logging;
#[cfg(feature = "mcp")]
mod mcp;
mod options;
mod proxy;

use clap::Parser;
use codespan_reporting::diagnostic::Diagnostic;
use micromux::{Printer as DiagnosticsPrinter, ToDiagnostics};

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Micromux(#[from] micromux::Error),
    #[error(transparent)]
    Tui(#[from] micromux_tui::Error),
    #[error(transparent)]
    Control(#[from] micromux_control::ControlError),
    #[error(transparent)]
    Session(#[from] micromux::SessionError),
    #[cfg(unix)]
    #[error(transparent)]
    Proxy(#[from] micromux_proxy::ClientError),
    #[error(transparent)]
    Diagnostics(#[from] codespan_reporting::files::Error),
    #[error(transparent)]
    Logging(#[from] logging::Error),
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
    #[cfg(feature = "mcp")]
    #[error("mcp server error: {0}")]
    Mcp(Box<dyn std::error::Error + Send + Sync>),
    #[error("{0}")]
    Message(String),
    /// The failure was already explained by diagnostics printed to stderr.
    #[error("configuration errors were reported")]
    DiagnosticsEmitted,
}

fn spawn_shutdown_handler(shutdown: micromux::CancellationToken) {
    tokio::spawn(async move {
        let ctrl_c = async {
            let _ = tokio::signal::ctrl_c().await;
        };

        #[cfg(unix)]
        let terminate = async {
            use tokio::signal::unix::{SignalKind, signal};
            match signal(SignalKind::terminate()) {
                Ok(mut sigterm) => {
                    sigterm.recv().await;
                }
                Err(err) => {
                    tracing::warn!(?err, "failed to install SIGTERM handler");
                    std::future::pending::<()>().await;
                }
            }
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        #[cfg(unix)]
        let hangup = async {
            use tokio::signal::unix::{SignalKind, signal};
            match signal(SignalKind::hangup()) {
                Ok(mut sighup) => {
                    sighup.recv().await;
                }
                Err(err) => {
                    tracing::warn!(?err, "failed to install SIGHUP handler");
                    std::future::pending::<()>().await;
                }
            }
        };

        #[cfg(not(unix))]
        let hangup = std::future::pending::<()>();

        tokio::select! {
            () = ctrl_c => {},
            () = terminate => {},
            () = hangup => {},
        }

        shutdown.cancel();
    });
}

#[cfg(unix)]
fn raise_file_descriptor_limit() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

    // A live run owns several PTY, cancellation, and log descriptors. macOS commonly starts at
    // 256; its PTY readiness path uses kqueue, so raising the limit does not hit select's ceiling.
    const TARGET: u64 = 4096;
    let current = getrlimit(Resource::Nofile);
    let desired = current
        .maximum
        .map_or(TARGET, |maximum| TARGET.min(maximum));
    if current.current.is_none_or(|limit| limit >= desired) {
        return;
    }
    if let Err(err) = setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(desired),
            maximum: current.maximum,
        },
    ) {
        eprintln!("warning: could not raise the open-file limit to {desired}: {err}");
    }
}

#[cfg(not(unix))]
fn raise_file_descriptor_limit() {}

/// Derive an effective log level from the configured `--log` level and the `-v`/`-q` counts.
///
/// `--log`/`RUST_LOG` take precedence; otherwise `-v`/`-q` adjust a `WARN` baseline.
fn effective_log_level(options: &options::Options) -> tracing::metadata::Level {
    use tracing::metadata::Level;
    if let Some(level) = options.log_level {
        return level;
    }
    let delta = i16::from(options.verbosity.verbose) - i16::from(options.verbosity.quiet);
    match delta {
        i16::MIN..=-1 => Level::ERROR,
        0 => Level::WARN,
        1 => Level::INFO,
        2 => Level::DEBUG,
        _ => Level::TRACE,
    }
}

fn setup_logging(
    options: &options::Options,
) -> Result<tracing_appender::non_blocking::WorkerGuard, Error> {
    let project_dir = micromux::project_dir()
        .ok_or_else(|| Error::Message("failed to create project directory".to_string()))?;
    let log_file = match options.log_file.as_deref() {
        Some(path) => logging::LogFile::LogFile { path },
        None => logging::LogFile::RollingLog {
            cache_dir: project_dir.cache_dir(),
        },
    };
    let guard = logging::setup(Some(effective_log_level(options)), &log_file)?;
    Ok(guard)
}

/// A parsed config together with the printer that holds its source text, so later stages can
/// still render diagnostics anchored in the file.
struct LoadedConfig {
    config: micromux::ConfigFile<usize>,
    printer: DiagnosticsPrinter,
}

/// Build the supervisor, rendering every service definition that cannot be materialized (an
/// unresolved `${VAR}`, an unreadable env file, an invalid port) and every route that cannot be
/// resolved as a source diagnostic, preceded by notes for the optional env files that were
/// skipped.
fn build_supervisor(
    config: &micromux::ConfigFile<usize>,
    printer: &DiagnosticsPrinter,
    session: micromux::Session,
) -> Result<micromux::Micromux, Error> {
    let diagnostics = match micromux::Micromux::with_session(config, session) {
        Ok(mux) => return Ok(mux),
        Err(micromux::Error::Services(failure)) => failure.to_diagnostics(config.file_id),
        Err(micromux::Error::Routes(failure)) => failure.to_diagnostics(config.file_id),
        Err(err) => return Err(err.into()),
    };
    for diagnostic in diagnostics {
        printer.emit(&diagnostic)?;
    }
    Err(Error::DiagnosticsEmitted)
}

/// A supervisor whose session is established and whose routes are registered, ready to start
/// services.
struct Supervisor {
    mux: std::sync::Arc<micromux::Micromux>,
    /// Held so the routes stay registered while the session runs.
    routes: Option<proxy::RouteRegistration>,
}

/// Establishes the session (identity, allocated ports, and the proxy port), builds the
/// supervisor, and registers its routes before any service starts.
async fn start_supervisor(
    config: &micromux::ConfigFile<usize>,
    printer: &DiagnosticsPrinter,
    shutdown: &micromux::CancellationToken,
) -> Result<Supervisor, Error> {
    let mut session = micromux::Session::establish(config, &micromux::PortRegistry::user())?;
    proxy::latch_port(&mut session).await?;
    let mux = build_supervisor(config, printer, session)?;
    let routes = match config.config_path.as_deref() {
        Some(config_path) => proxy::register(&mux, config_path, shutdown).await?,
        None => None,
    };
    Ok(Supervisor {
        mux: std::sync::Arc::new(mux),
        routes,
    })
}

async fn load_config(
    options: &options::Options,
    color_choice: termcolor::ColorChoice,
) -> Result<LoadedConfig, Error> {
    let working_dir = std::env::current_dir()?;
    let config_path = match options.config_path.as_ref() {
        Some(config_path) => Some(config_path.clone()),
        None => micromux::find_config_file(&working_dir).await?,
    };
    let config_path = config_path
        .ok_or_else(|| Error::Message("missing config file".to_string()))?
        .canonicalize()?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| Error::Message("failed to get config file".to_string()))?;

    let raw_config = micromux::read_config_file_async(&config_path).await?;

    let diagnostic_printer = DiagnosticsPrinter::new(color_choice);
    let file_id = diagnostic_printer.add_source_file(&config_path, raw_config.clone());
    let mut diagnostics: Vec<Diagnostic<usize>> = vec![];

    let config = match micromux::from_str(
        &raw_config,
        config_dir,
        file_id,
        options.strict,
        &mut diagnostics,
    ) {
        Err(err) => {
            diagnostics.extend(err.to_diagnostics(file_id));
            None
        }
        Ok(config) => Some(config),
    };

    let has_error = diagnostics
        .iter()
        .any(|d| d.severity == codespan_reporting::diagnostic::Severity::Error);
    for diagnostic in diagnostics {
        diagnostic_printer.emit(&diagnostic)?;
    }

    let Some(mut config) = config else {
        return Err(Error::DiagnosticsEmitted);
    };
    if has_error {
        return Err(Error::DiagnosticsEmitted);
    }

    config.config_path = Some(config_path);
    Ok(LoadedConfig {
        config,
        printer: diagnostic_printer,
    })
}

/// Runs a subcommand; without one, micromux runs the TUI.
async fn run_command(command: options::Command, options: options::Options) -> Result<(), Error> {
    match command {
        options::Command::Attach { session } => attach::run(&options, session.as_deref()).await,
        options::Command::Ctl { action } => ctl::run(action, options.config_path.as_deref()).await,
        #[cfg(feature = "mcp")]
        options::Command::Mcp {
            allow_session_start,
        } => {
            // Logs go to a file (never stdout — stdout is the JSON-RPC channel).
            let _log_guard = setup_logging(&options).ok();
            mcp::run(allow_session_start).await
        }
        options::Command::Serve => run_headless(options).await,
        options::Command::Proxy {
            port,
            fallback_port,
            idle_timeout_secs,
        } => {
            // Logs go to a file: a session that started the proxy reads only its startup errors.
            let _log_guard = setup_logging(&options).ok();
            proxy::run(
                port,
                fallback_port,
                std::time::Duration::from_secs(idle_timeout_secs),
            )
            .await
        }
    }
}

async fn run() -> Result<(), Error> {
    let mut options = options::Options::parse();

    if let Some(command) = options.command.take() {
        return run_command(command, options).await;
    }

    let shutdown = micromux::CancellationToken::new();
    spawn_shutdown_handler(shutdown.clone());

    let color_choice = options.color_choice.unwrap_or(termcolor::ColorChoice::Auto);
    // Hold the guard for the whole program: dropping it shuts down the non-blocking log writer
    // thread, after which nothing is written to the log file.
    let _log_guard = setup_logging(&options)?;

    let LoadedConfig { config, printer } = load_config(&options, color_choice).await?;

    let Supervisor { mux, routes } = start_supervisor(&config, &printer, &shutdown).await?;
    let (runner, handles) = mux.clone().start(shutdown.clone());
    if let Some(routes) = &routes {
        routes.report_to(handles.session.clone(), shutdown.clone());
    }

    // Default-on control plane, opt out via `--no-control` or `control: { enabled: false }`.
    let mut control_warning = None;
    if !options.no_control && config.config.control.enabled {
        let working_dir = std::env::current_dir()?;
        match control::resolve_config_path(options.config_path.as_deref(), &working_dir).await {
            Ok(config_path) => {
                match control::spawn(
                    &handles,
                    &config_path,
                    &working_dir,
                    config.config.name.clone(),
                    shutdown.clone(),
                ) {
                    control::SpawnStatus::Started => {}
                    control::SpawnStatus::OwnedByOther => {
                        control_warning = Some(
                        "another micromux already owns this project; both copies may run services"
                            .to_string(),
                        );
                    }
                    control::SpawnStatus::Unavailable => {
                        control_warning =
                            Some("the control plane is unavailable for this session".to_string());
                    }
                }
            }
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "control plane disabled: could not resolve config path"
                );
                control_warning =
                    Some("the control plane is unavailable for this session".to_string());
            }
        }
    }

    let input = handles.terminal.clone();
    let mut source =
        micromux_tui::LocalSource::new(handles.reader.clone(), handles.commands.clone())
            .with_session(handles.session.clone());
    if let Some(warning) = control_warning {
        source = source.with_notice(warning);
    }
    let tui = micromux_tui::App::new(
        micromux_tui::SessionSource::Local(source),
        Some(input),
        shutdown.clone(),
        micromux_tui::LogViewOptions {
            pretty_json: config.config.ui_config.pretty_json_logs && !options.no_pretty_json_logs,
        },
    );

    let tui_handle = tokio::task::spawn(async move { tui.render().await });

    let mux_handle = tokio::task::spawn(runner);

    let mut tui_handle = tui_handle;
    let mut mux_handle = mux_handle;

    tokio::select! {
        render_res = &mut tui_handle => {
            shutdown.cancel();
            let mux_res = mux_handle.await;
            render_res??;
            mux_res??;
        }
        mux_res = &mut mux_handle => {
            shutdown.cancel();
            // Let the TUI exit its loop and restore the terminal before propagating any
            // scheduler error, otherwise the terminal is left in raw mode / alternate screen.
            let tui_res = tui_handle.await;
            mux_res??;
            tui_res??;
        }
    }

    Ok(())
}

/// Run the supervisor headless: scheduler + control plane, no TUI, until shutdown (a signal or a
/// `stop_session`/`Shutdown` request). Intended for agent-managed sessions spawned by the MCP
/// `start_session` tool.
async fn run_headless(options: options::Options) -> Result<(), Error> {
    let shutdown = micromux::CancellationToken::new();
    spawn_shutdown_handler(shutdown.clone());

    // Hold the guard for the whole run: dropping it stops the non-blocking log writer.
    let _log_guard = setup_logging(&options)?;
    let color_choice = options.color_choice.unwrap_or(termcolor::ColorChoice::Auto);
    let LoadedConfig { config, printer } = load_config(&options, color_choice).await?;

    let Supervisor { mux, routes } = start_supervisor(&config, &printer, &shutdown).await?;
    let (runner, handles) = mux.clone().start(shutdown.clone());
    if let Some(routes) = &routes {
        routes.report_to(handles.session.clone(), shutdown.clone());
    }

    // The control plane is the only way to reach a headless session, so it is mandatory here — the
    // `--no-control` / `control.enabled` opt-out applies to the TUI, not to `serve`.
    let working_dir = std::env::current_dir()?;
    let config_path =
        control::resolve_config_path(options.config_path.as_deref(), &working_dir).await?;
    let status = control::spawn(
        &handles,
        &config_path,
        &working_dir,
        config.config.name.clone(),
        shutdown.clone(),
    );
    if !matches!(status, control::SpawnStatus::Started) {
        // Another live session already owns this project (or there is no transport): a headless
        // session with no reachable control plane is useless, so don't even start the scheduler.
        return Err(Error::Message(
            "could not start the control plane for `serve` (another micromux may already own this project)"
                .to_string(),
        ));
    }

    tracing::info!(config = %config_path.display(), "micromux serve: headless supervisor running");
    tokio::spawn(runner).await??;
    Ok(())
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    raise_file_descriptor_limit();
    match run().await {
        Ok(()) => Ok(()),
        // The diagnostics already explained the failure; a report with a location and a
        // backtrace hint would make a config typo read like a crash.
        Err(Error::DiagnosticsEmitted) => std::process::exit(1),
        Err(err) => Err(err.into()),
    }
}
