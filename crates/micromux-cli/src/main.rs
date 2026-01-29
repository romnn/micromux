pub mod logging;
pub mod options;

use clap::Parser;
use codespan_reporting::diagnostic::Diagnostic;
use color_eyre::eyre;
use micromux::{Printer as DiagnosticsPrinter, ToDiagnostics};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    use termcolor::ColorChoice;

    color_eyre::install()?;
    let options = options::Options::parse();

    let shutdown = micromux::CancellationToken::new();

    // Wire OS shutdown signals into the shared cancellation token.
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
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
        }
    });

    // setup logging to a log file
    let project_dir =
        micromux::project_dir().ok_or_else(|| eyre::eyre!("failed to create project directory"))?;
    let log_file = match options.log_file.as_deref() {
        Some(path) => logging::LogFile::LogFile { path },
        None => logging::LogFile::RollingLog {
            cache_dir: project_dir.cache_dir(),
        },
    };

    let color_choice = options.color_choice.unwrap_or(termcolor::ColorChoice::Auto);
    let _use_color = match color_choice {
        ColorChoice::Always | ColorChoice::AlwaysAnsi => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            use std::io::IsTerminal;
            std::io::stdout().is_terminal()
        }
    };

    let _guard = logging::setup(options.log_level, log_file)?;

    let working_dir = std::env::current_dir()?;
    let config_path = match options.config_path {
        Some(config_path) => Some(config_path),
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
    let file_id = diagnostic_printer
        .add_source_file(&config_path, raw_config.clone())
        .await;
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

    // emit diagnostics
    let has_error = diagnostics
        .iter()
        .any(|d| d.severity == codespan_reporting::diagnostic::Severity::Error);
    for diagnostic in diagnostics.into_iter() {
        diagnostic_printer.emit(&diagnostic).await?;
    }
    let Some(config) = config else {
        eyre::bail!("failed to parse config");
    };
    if has_error {
        eyre::bail!("failed to parse config");
    }

    let (ui_tx, ui_rx) = mpsc::channel(1024);
    let (commands_tx, commands_rx) = mpsc::channel(1024);
    let mux = micromux::Micromux::new(config)?;
    let services = mux.services();
    let tui = micromux_tui::App::new(&services, ui_rx, commands_tx, shutdown.clone());
    let interactive_logs = !options.no_interactive_logs;
    let mux_handle = tokio::task::spawn({
        async move {
            mux.start_with_options(ui_tx, commands_rx, shutdown.clone(), interactive_logs)
                .await
        }
    });
    let (render_res, mux_res) = futures::join!(tui.render(), mux_handle);
    render_res?;
    mux_res??;
    Ok(())
}
