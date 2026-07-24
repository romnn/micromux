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
}

fn spawn_shutdown_handler(shutdown: micromux::CancellationToken) {
    tokio::spawn(async move {
        let ctrl_c = async {
            let _ = tokio::signal::ctrl_c().await;
        };

        #[cfg(unix)]
        let terminate = async {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                sigterm.recv().await;
            }
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            () = ctrl_c => {},
            () = terminate => {},
        }

        shutdown.cancel();
    });
}

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

async fn load_config(
    options: &options::Options,
    color_choice: termcolor::ColorChoice,
) -> Result<micromux::ConfigFile<usize>, Error> {
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

    let raw_config = tokio::fs::read_to_string(&config_path).await?;

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
        return Err(Error::Message("failed to parse config".to_string()));
    };
    if has_error {
        return Err(Error::Message("failed to parse config".to_string()));
    }

    config.config_path = Some(config_path);
    Ok(config)
}

async fn run() -> Result<(), Error> {
    let mut options = options::Options::parse();

    match options.command.take() {
        Some(options::Command::Attach { session }) => {
            return attach::run(&options, session.as_deref()).await;
        }
        Some(options::Command::Ctl { action }) => {
            return ctl::run(action, options.config_path.as_deref()).await;
        }
        #[cfg(feature = "mcp")]
        Some(options::Command::Mcp) => {
            // Logs go to a file (never stdout — stdout is the JSON-RPC channel).
            let _log_guard = setup_logging(&options).ok();
            return mcp::run().await;
        }
        Some(options::Command::Serve) => {
            return run_headless(options).await;
        }
        None => {}
    }

    let shutdown = micromux::CancellationToken::new();
    spawn_shutdown_handler(shutdown.clone());

    let color_choice = options.color_choice.unwrap_or(termcolor::ColorChoice::Auto);
    // Hold the guard for the whole program: dropping it shuts down the non-blocking log writer
    // thread, after which nothing is written to the log file.
    let _log_guard = setup_logging(&options)?;

    let config = load_config(&options, color_choice).await?;

    let mux = std::sync::Arc::new(micromux::Micromux::new(&config)?);
    let (runner, handles) = mux.clone().start(shutdown.clone());

    // Default-on control plane, opt out via `--no-control` or `control: { enabled: false }`.
    if !options.no_control && config.config.control.enabled {
        let working_dir = std::env::current_dir()?;
        match control::resolve_config_path(options.config_path.as_deref(), &working_dir).await {
            Ok(config_path) => {
                control::spawn(
                    &handles,
                    &config_path,
                    &working_dir,
                    config.config.name.clone(),
                    shutdown.clone(),
                );
            }
            Err(err) => tracing::warn!(
                ?err,
                "control plane disabled: could not resolve config path"
            ),
        }
    }

    let input = handles.commands.clone();
    let tui = micromux_tui::App::new(
        micromux_tui::SessionSource::Local(micromux_tui::LocalSource::new(
            handles.reader.clone(),
            handles.commands.clone(),
        )),
        Some(input),
        shutdown.clone(),
        config.config.ui_config.pretty_json_logs && !options.no_pretty_json_logs,
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
    let config = load_config(&options, color_choice).await?;

    let mux = std::sync::Arc::new(micromux::Micromux::new(&config)?);
    let (runner, handles) = mux.clone().start(shutdown.clone());

    // The control plane is the only way to reach a headless session, so it is mandatory here — the
    // `--no-control` / `control.enabled` opt-out applies to the TUI, not to `serve`.
    let working_dir = std::env::current_dir()?;
    let config_path =
        control::resolve_config_path(options.config_path.as_deref(), &working_dir).await?;
    let bound = control::spawn(
        &handles,
        &config_path,
        &working_dir,
        config.config.name.clone(),
        shutdown.clone(),
    );
    if !bound {
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
    Ok(run().await?)
}
