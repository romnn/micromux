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
mod session_env;

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
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[cfg(feature = "mcp")]
    #[error("mcp server error: {0}")]
    Mcp(Box<dyn std::error::Error + Send + Sync>),
    #[error("{0}")]
    Message(String),
    /// The failure was already explained by diagnostics printed to stderr.
    #[error("configuration errors were reported")]
    DiagnosticsEmitted,
    /// The failure was already reported on stderr, such as a `ctl wait` that ended without its
    /// condition; the process exits with this code.
    #[error("the command failed with exit code {0}")]
    Exit(i32),
    /// The session was asked to stop before it started.
    #[error(transparent)]
    Interrupted(#[from] micromux::CleanupWaitCancelled),
}

/// Exit status of a session that an interrupt ended before it could stop cleanly, as a shell
/// reports `SIGINT`.
const EXIT_INTERRUPTED: i32 = 130;

fn spawn_shutdown_handler(shutdown: micromux::CancellationToken) {
    spawn_second_interrupt_handler(shutdown.clone());
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

/// Exits at once on the first Ctrl-C after the shutdown started, however it started.
///
/// The service shims notice the supervisor is gone and finish the remaining stops and cleanups
/// themselves, each bounded by its stop grace period or cleanup timeout.
fn spawn_second_interrupt_handler(shutdown: micromux::CancellationToken) {
    tokio::spawn(async move {
        shutdown.cancelled().await;
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("exiting; service shims finish the remaining stops and cleanups");
            std::process::exit(EXIT_INTERRUPTED);
        }
    });
}

/// The services of `config` that run a cleanup after each of their processes exits.
fn services_with_cleanup(config: &micromux::ConfigFile<usize>) -> Vec<micromux::ServiceID> {
    config
        .config
        .services
        .iter()
        .filter(|(_, service)| service.cleanup.is_some())
        .map(|(id, _)| id.as_ref().clone())
        .collect()
}

/// Says on stderr which of `with_cleanup` still run a cleanup before the session can exit.
///
/// Only services with a live run stop with a cleanup; a run that already ended ran its own.
fn report_pending_cleanups(
    with_cleanup: &[micromux::ServiceID],
    reader: &micromux::SessionModelReader,
) {
    let pending = with_cleanup
        .iter()
        .filter(|id| {
            reader.service(id).is_some_and(|snapshot| {
                matches!(
                    snapshot.execution,
                    micromux::Execution::Starting
                        | micromux::Execution::Running
                        | micromux::Execution::Stopping
                )
            })
        })
        .map(String::as_str)
        .collect::<Vec<_>>();
    if !pending.is_empty() {
        eprintln!(
            "stopping; waiting for the cleanup of {} (press Ctrl-C again to exit at once)",
            pending.join(", ")
        );
    }
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
    let cache_dir = micromux::cache_directory()
        .ok_or_else(|| Error::Message("failed to create project directory".to_string()))?;
    let log_file = match options.log_file.as_deref() {
        Some(path) => logging::LogFile::LogFile { path },
        None => logging::LogFile::RollingLog {
            cache_dir: &cache_dir,
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

/// The executable that runs the containment shim: this very binary.
///
/// On Linux `/proc/self/exe`, resolved by each child before it execs, names the running image
/// even after the installed binary was moved or replaced by an upgrade, so a long-running
/// session keeps starting the shim it was built with.
fn shim_executable() -> Result<std::path::PathBuf, Error> {
    if cfg!(target_os = "linux") {
        Ok(std::path::PathBuf::from("/proc/self/exe"))
    } else {
        Ok(std::env::current_exe()?)
    }
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
    // Every service and healthcheck runs under the containment shim, so their process trees die
    // with this supervisor even when it is SIGKILLed.
    micromux::containment::enable(shim_executable()?);
    // A previous session that died may have left cleanups to its shims, which must finish before
    // this session starts the same services again.
    micromux::wait_for_previous_cleanups(config, shutdown, |busy| {
        eprintln!(
            "waiting for the cleanup of {} from the previous session",
            busy.join(", ")
        );
    })
    .await?;
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
        options::Command::Env {
            session,
            service,
            export,
            format,
            names,
        } => {
            let target = session_env::Target {
                session: session.as_deref(),
                service: service.as_deref(),
                config: options.config_path.as_deref(),
            };
            let format = if export {
                options::EnvFormat::Shell
            } else {
                format.unwrap_or(options::EnvFormat::Env)
            };
            session_env::print(
                &target,
                &session_env::Output {
                    format,
                    names: &names,
                },
            )
            .await
        }
        options::Command::Exec {
            session,
            service,
            command,
        } => {
            let target = session_env::Target {
                session: session.as_deref(),
                service: service.as_deref(),
                config: options.config_path.as_deref(),
            };
            session_env::exec(&target, &command).await
        }
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
    // Every micromux of an isolated setup must agree on its directory, wherever it starts.
    if let Some(state) = std::env::var_os(micromux::STATE_DIR_VARIABLE)
        && !state.is_empty()
        && !std::path::Path::new(&state).is_absolute()
    {
        return Err(Error::Message(format!(
            "{} must be an absolute path, not `{}`",
            micromux::STATE_DIR_VARIABLE,
            std::path::Path::new(&state).display()
        )));
    }
    // `exec` replaces this process, and the command it runs keeps the caller's open-file limit.
    if !matches!(options.command, Some(options::Command::Exec { .. })) {
        raise_file_descriptor_limit();
    }

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
            // The TUI has given the terminal back, so stderr is readable again.
            report_pending_cleanups(&services_with_cleanup(&config), &handles.reader);
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
    tokio::spawn({
        let shutdown = shutdown.clone();
        let reader = handles.reader.clone();
        let with_cleanup = services_with_cleanup(&config);
        async move {
            shutdown.cancelled().await;
            report_pending_cleanups(&with_cleanup, &reader);
        }
    });
    tokio::spawn(runner).await??;
    Ok(())
}

fn main() -> color_eyre::Result<()> {
    // The containment shim wraps every service and healthcheck command, so it starts before any
    // runtime, logging, or argument parsing: one small single-threaded process per command.
    let mut args = std::env::args_os();
    if args.nth(1).as_deref() == Some(std::ffi::OsStr::new(micromux::containment::SHIM_COMMAND)) {
        std::process::exit(micromux::containment::run_shim(args));
    }
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(async_main());
    // Dropping the runtime would wait for every blocking task, such as a child wait that never
    // completes; the session has drained by now, so whatever is left is abandoned.
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
    result
}

/// How long the runtime waits for leftover tasks once the session ended.
const RUNTIME_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

async fn async_main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    match run().await {
        Ok(()) => Ok(()),
        // The diagnostics already explained the failure; a report with a location and a
        // backtrace hint would make a config typo read like a crash.
        Err(Error::DiagnosticsEmitted) => std::process::exit(1),
        Err(Error::Interrupted(_)) => std::process::exit(EXIT_INTERRUPTED),
        #[cfg(unix)]
        Err(Error::Proxy(micromux_proxy::ClientError::Cancelled)) => {
            std::process::exit(EXIT_INTERRUPTED)
        }
        Err(Error::Exit(code)) => std::process::exit(code),
        Err(err) => Err(err.into()),
    }
}
