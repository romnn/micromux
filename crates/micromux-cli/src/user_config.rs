//! Shared user configuration loading for the TUI and session metadata policy.

use crate::{DiagnosticsPrinter, Error};

/// Reads the user config, printing its diagnostics; a missing file is an empty config.
pub(crate) async fn load(printer: &DiagnosticsPrinter) -> Result<micromux::UserConfig, Error> {
    let Some(path) = micromux::user_config_path() else {
        return Ok(micromux::UserConfig::default());
    };
    let raw = match micromux::read_config_file_async(&path).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(micromux::UserConfig::default());
        }
        Err(err) => {
            return Err(Error::Message(format!(
                "cannot read the user config {}: {err}",
                path.display()
            )));
        }
    };
    let file_id = printer.add_source_file(&path, raw.clone());
    let mut diagnostics = Vec::new();
    let parsed = micromux::parse_user_config(&raw, file_id, &mut diagnostics);
    if let Err(err) = &parsed {
        diagnostics.extend(micromux::ToDiagnostics::to_diagnostics(err, file_id));
    }
    for diagnostic in &diagnostics {
        printer.emit(diagnostic)?;
    }
    parsed.map_err(|_| Error::DiagnosticsEmitted)
}

/// Resolves the project override before consulting the user's default.
pub(crate) async fn git_metadata(
    project: Option<bool>,
    printer: &DiagnosticsPrinter,
) -> Result<bool, Error> {
    match project {
        Some(enabled) => Ok(enabled),
        None => Ok(load(printer).await?.git_metadata.unwrap_or(true)),
    }
}
