//! `micromux-cli` is the command-line entry point for micromux.
//!
//! It is responsible for:
//! - Locating and parsing the configuration file.
//! - Emitting diagnostics.
//! - Starting the TUI and scheduler.

mod control;
mod ctl;
mod logging;
#[cfg(feature = "mcp")]
mod mcp;
mod options;

use clap::Parser;
use codespan_reporting::diagnostic::Diagnostic;
use color_eyre::eyre;
use micromux::{Printer as DiagnosticsPrinter, ToDiagnostics};
use tokio::sync::mpsc;

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
) -> eyre::Result<tracing_appender::non_blocking::WorkerGuard> {
    let project_dir =
        micromux::project_dir().ok_or_else(|| eyre::eyre!("failed to create project directory"))?;
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
) -> eyre::Result<micromux::ConfigFile<usize>> {
    let working_dir = std::env::current_dir()?;
    let config_path = match options.config_path.as_ref() {
        Some(config_path) => Some(config_path.clone()),
        None => micromux::find_config_file(&working_dir).await?,
    };
    let config_path = config_path
        .ok_or_else(|| eyre::eyre!("missing config file"))?
        .canonicalize()?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| eyre::eyre!("failed to get config file"))?;

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

    let Some(config) = config else {
        eyre::bail!("failed to parse config");
    };
    if has_error {
        eyre::bail!("failed to parse config");
    }

    Ok(config)
}

async fn run() -> eyre::Result<()> {
    color_eyre::install()?;
    let mut options = options::Options::parse();

    match options.command.take() {
        Some(options::Command::Ctl { action }) => {
            return ctl::run(action, options.config_path.as_deref()).await;
        }
        #[cfg(feature = "mcp")]
        Some(options::Command::Mcp) => {
            // Logs go to a file (never stdout — stdout is the JSON-RPC channel).
            let _log_guard = setup_logging(&options).ok();
            return mcp::run().await;
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

    let (ui_tx, ui_rx) = mpsc::channel(1024);
    let mux = std::sync::Arc::new(micromux::Micromux::new(&config)?);
    let services = mux.services();
    let (runner, handles) = mux.clone().start_with_handles(ui_tx, shutdown.clone());

    // The TUI now reads the authoritative model directly; drain the legacy event channel so the
    // scheduler's bridge stays healthy (dropping the receiver would cancel the session). Retiring
    // the granular event path entirely is future work.
    tokio::spawn(async move {
        let mut ui_rx = ui_rx;
        while ui_rx.recv().await.is_some() {}
    });

    // Default-on control plane, opt out via `--no-control` or `control: { enabled: false }`.
    if !options.no_control && config.config.control_enabled {
        let working_dir = std::env::current_dir()?;
        match control::resolve_config_path(options.config_path.as_deref(), &working_dir).await {
            Ok(config_path) => control::spawn(
                &handles,
                &config_path,
                &working_dir,
                config.config.name.clone(),
                shutdown.clone(),
            ),
            Err(err) => tracing::warn!(
                ?err,
                "control plane disabled: could not resolve config path"
            ),
        }
    }

    let tui = micromux_tui::App::new(
        &services,
        handles.reader.clone(),
        handles.commands.clone(),
        shutdown.clone(),
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

#[tokio::main]
async fn main() -> eyre::Result<()> {
    run().await
}
