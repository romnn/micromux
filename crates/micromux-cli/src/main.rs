//! `micromux-cli` is the command-line entry point for micromux.
//!
//! It is responsible for:
//! - Locating and parsing the configuration file.
//! - Emitting diagnostics.
//! - Starting the TUI and scheduler.

mod logging;
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

fn setup_logging(options: &options::Options) -> eyre::Result<()> {
    let project_dir =
        micromux::project_dir().ok_or_else(|| eyre::eyre!("failed to create project directory"))?;
    let log_file = match options.log_file.as_deref() {
        Some(path) => logging::LogFile::LogFile { path },
        None => logging::LogFile::RollingLog {
            cache_dir: project_dir.cache_dir(),
        },
    };
    let _guard = logging::setup(options.log_level, &log_file)?;
    Ok(())
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
    let options = options::Options::parse();
    let shutdown = micromux::CancellationToken::new();
    spawn_shutdown_handler(shutdown.clone());

    let color_choice = options.color_choice.unwrap_or(termcolor::ColorChoice::Auto);
    setup_logging(&options)?;

    let config = load_config(&options, color_choice).await?;

    let (ui_tx, ui_rx) = mpsc::channel(1024);
    let (commands_tx, commands_rx) = mpsc::channel(1024);
    let mux = micromux::Micromux::new(&config)?;
    let services = mux.services();
    let tui = micromux_tui::App::new(&services, ui_rx, commands_tx, shutdown.clone());

    let tui_handle = tokio::task::spawn(async move { tui.render().await });

    let mux_handle = tokio::task::spawn({
        let shutdown = shutdown.clone();
        async move { mux.start(ui_tx, commands_rx, shutdown).await }
    });

    let mut tui_handle = tui_handle;
    let mut mux_handle = mux_handle;

    tokio::select! {
        render_res = &mut tui_handle => {
            shutdown.cancel();
            render_res??;
            mux_handle.await??;
        }
        mux_res = &mut mux_handle => {
            shutdown.cancel();
            mux_res??;
            tui_handle.await??;
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    run().await
}
