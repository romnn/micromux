//! Whether the TUI captures the mouse.
//!
//! The first setting that says wins:
//!
//! 1. The `--mouse` flag, or `MICROMUX_MOUSE`.
//! 2. The project's `ui.mouse` in `micromux.yaml`.
//! 3. The user's `ui.mouse` in the user config.
//! 4. On.

use std::path::Path;

use crate::{DiagnosticsPrinter, Error};

/// Mouse capture when no setting says otherwise.
///
/// It is on because the TUI selects and copies text inside one pane itself, so capturing costs
/// no text selection, and because the wheel and clicks are how most people expect to scroll and
/// pick panes.
const DEFAULT_MOUSE_CAPTURE: bool = true;

/// Whether the TUI captures the mouse, given the flag, the project's setting and the user's.
fn resolve(flag: Option<bool>, project: Option<bool>, user: Option<bool>) -> bool {
    flag.or(project).or(user).unwrap_or(DEFAULT_MOUSE_CAPTURE)
}

/// Whether the TUI captures the mouse; the user config is read only when neither the flag nor
/// the project decides.
///
/// # Errors
///
/// Returns an error when the user config is needed but cannot be read, or when it is invalid,
/// after printing its diagnostics with `printer`.
pub(crate) async fn mouse_capture(
    flag: Option<bool>,
    project: Option<bool>,
    printer: &DiagnosticsPrinter,
) -> Result<bool, Error> {
    let user = if flag.or(project).is_some() {
        None
    } else {
        load_user_config(printer).await?.mouse
    };
    Ok(resolve(flag, project, user))
}

/// Reads the user config, printing its diagnostics; a missing file is an empty config.
async fn load_user_config(printer: &DiagnosticsPrinter) -> Result<micromux::UserConfig, Error> {
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

/// The `ui.mouse` setting of the project config at `config_path`, or `None` when it has none or
/// cannot be read.
///
/// An attached TUI reads it only for this setting: the session it attaches to already loaded and
/// reported on the config, so a problem with it is not this client's to report.
pub(crate) async fn project_mouse_setting(config_path: &Path) -> Option<bool> {
    let raw = micromux::read_config_file_async(config_path).await.ok()?;
    let config_dir = config_path.parent()?;
    let mut diagnostics = Vec::new();
    micromux::from_str(&raw, config_dir, 0usize, None, &mut diagnostics)
        .ok()?
        .config
        .ui_config
        .mouse
}

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn the_flag_beats_the_project_which_beats_the_user() {
        // Each setting decides only when every one above it is silent.
        assert!(!resolve(Some(false), Some(true), Some(true)));
        assert!(resolve(Some(true), Some(false), Some(false)));
        assert!(!resolve(None, Some(false), Some(true)));
        assert!(resolve(None, Some(true), Some(false)));
        assert!(!resolve(None, None, Some(false)));

        // With no setting at all, the mouse is captured.
        assert!(resolve(None, None, None));
    }
}
