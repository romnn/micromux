#![allow(warnings)]
#![deny(unused_must_use)]

pub mod logging;
pub mod options;

use clap::Parser;
use codespan_reporting::diagnostic::Diagnostic;
use color_eyre::eyre;
use micromux::{diagnostics::Printer as DiagnosticsPrinter, project_dir};
use std::sync::Arc;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    use termcolor::ColorChoice;

    color_eyre::install()?;
    let options = options::Options::parse();

    let shutdown = micromux::CancellationToken::new();

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
    let use_color = match color_choice {
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
        None => micromux::config::find_config_file(&working_dir).await?,
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

    let config = match micromux::config::from_str(
        &raw_config,
        config_dir,
        file_id,
        options.strict,
        &mut diagnostics,
    ) {
        Err(err) => {
            use micromux::diagnostics::ToDiagnostics;
            diagnostics.extend(err.to_diagnostics(file_id));
            // print them
            return Ok(());
            // Ok::<_, eyre::Report>((, diagnostics))
        }
        // Ok(valid_configs) => Ok::<_, eyre::Report>((valid_configs, diagnostics)),
        Ok(config) => config,
    };

    // emit diagnostics
    let has_error = diagnostics
        .iter()
        .any(|d| d.severity == codespan_reporting::diagnostic::Severity::Error);
    for diagnostic in diagnostics.into_iter() {
        diagnostic_printer.emit(&diagnostic).await?;
    }
    if has_error {
        eyre::bail!("failed to parse config");
    }

    dbg!(&config);

    let (ui_tx, ui_rx) = mpsc::channel(1024);
    let mux = micromux::Micromux::new(config)?;
    // let mux = Arc::new(mux);
    let tui = micromux_tui::App::new(&mux.services, ui_rx, shutdown.clone());
    let mux_handle = tokio::task::spawn({
        // let mux = Arc::clone(&app.mux);
        async move { mux.start(ui_tx, shutdown.clone()).await }
    });
    let (render_res, mux_res) = futures::join!(tui.render(), mux_handle);
    render_res?;
    mux_res??;
    Ok(())
}
