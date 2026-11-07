//! The TUI's mouse settings: whether it captures the mouse, and whether finishing a selection
//! copies it.
//!
//! For each setting, the first layer that says wins:
//!
//! 1. The command-line flag, or its environment variable.
//! 2. The project's `ui` section in `micromux.yaml`.
//! 3. The user's `ui` section in the user config.
//! 4. The default.

use std::path::Path;

use crate::{DiagnosticsPrinter, Error};

/// Mouse capture when no setting says otherwise.
///
/// It is on because the TUI selects and copies text inside one pane itself, so capturing costs
/// no text selection, and because the wheel and clicks are how most people expect to scroll and
/// pick panes.
const DEFAULT_MOUSE_CAPTURE: bool = true;

/// Copy-on-select when no setting says otherwise.
///
/// It is off because a selection that overwrites the clipboard by itself surprises people who
/// keep something there, so copying waits for `y` or Enter.
const DEFAULT_COPY_ON_SELECT: bool = false;

/// One setting from each layer, or `None` where that layer is silent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Layers {
    pub(crate) mouse: Option<bool>,
    pub(crate) copy_on_select: Option<bool>,
}

impl Layers {
    /// The settings of `self`, with those it leaves open taken from `below`.
    fn or(self, below: Self) -> Self {
        Self {
            mouse: self.mouse.or(below.mouse),
            copy_on_select: self.copy_on_select.or(below.copy_on_select),
        }
    }

    fn is_complete(self) -> bool {
        self.mouse.is_some() && self.copy_on_select.is_some()
    }

    fn from_project(ui: &micromux::UiConfig) -> Self {
        Self {
            mouse: ui.mouse,
            copy_on_select: ui.copy_on_select,
        }
    }
}

/// The settings that apply, given the flags, the project's settings and the user's.
fn resolve(flags: Layers, project: Layers, user: Layers) -> micromux_tui::MouseOptions {
    let settings = flags.or(project).or(user);
    micromux_tui::MouseOptions {
        capture: settings.mouse.unwrap_or(DEFAULT_MOUSE_CAPTURE),
        copy_on_select: settings.copy_on_select.unwrap_or(DEFAULT_COPY_ON_SELECT),
    }
}

/// The mouse settings that apply; the user config is read only when the flags and the project
/// leave a setting open.
///
/// # Errors
///
/// Returns an error when the user config is needed but cannot be read, or when it is invalid,
/// after printing its diagnostics with `printer`.
pub(crate) async fn mouse_options(
    flags: Layers,
    project: Option<&micromux::UiConfig>,
    printer: &DiagnosticsPrinter,
) -> Result<micromux_tui::MouseOptions, Error> {
    let project = project.map(Layers::from_project).unwrap_or_default();
    let user = if flags.or(project).is_complete() {
        Layers::default()
    } else {
        let user = load_user_config(printer).await?;
        Layers {
            mouse: user.mouse,
            copy_on_select: user.copy_on_select,
        }
    };
    Ok(resolve(flags, project, user))
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

/// The `ui` section of the project config at `config_path`, or `None` when it cannot be read.
///
/// An attached TUI reads only that section: the session it attaches to already loaded and
/// reported on the config, so the rest is not this client's to judge.
pub(crate) async fn project_ui_config(config_path: &Path) -> Option<micromux::UiConfig> {
    let raw = match micromux::read_config_file_async(config_path).await {
        Ok(raw) => raw,
        Err(err) => {
            tracing::warn!(?err, path = %config_path.display(), "cannot read the project's ui settings");
            return None;
        }
    };
    match micromux::parse_ui_section(&raw) {
        Ok(ui) => Some(ui),
        Err(err) => {
            tracing::warn!(?err, path = %config_path.display(), "cannot read the project's ui settings");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Layers, resolve};
    use similar_asserts::assert_eq;

    fn mouse(mouse: Option<bool>) -> Layers {
        Layers {
            mouse,
            copy_on_select: None,
        }
    }

    #[test]
    fn the_flag_beats_the_project_which_beats_the_user() {
        // Each layer decides only when every one above it is silent.
        let capture = |flags, project, user| resolve(mouse(flags), mouse(project), mouse(user));
        assert!(!capture(Some(false), Some(true), Some(true)).capture);
        assert!(capture(Some(true), Some(false), Some(false)).capture);
        assert!(!capture(None, Some(false), Some(true)).capture);
        assert!(capture(None, Some(true), Some(false)).capture);
        assert!(!capture(None, None, Some(false)).capture);

        // With no setting at all, the mouse is captured.
        assert!(capture(None, None, None).capture);
    }

    #[test]
    fn copy_on_select_is_off_unless_a_layer_turns_it_on() {
        let copy_on_select = |flags, project, user| {
            let layer = |copy_on_select| Layers {
                mouse: None,
                copy_on_select,
            };
            resolve(layer(flags), layer(project), layer(user)).copy_on_select
        };
        assert!(!copy_on_select(None, None, None));
        assert!(copy_on_select(None, None, Some(true)));
        assert!(!copy_on_select(None, Some(false), Some(true)));
        assert!(copy_on_select(Some(true), Some(false), None));

        // The two settings come from their own layers.
        let settings = resolve(
            mouse(Some(false)),
            Layers::default(),
            Layers {
                mouse: Some(true),
                copy_on_select: Some(true),
            },
        );
        assert_eq!((settings.capture, settings.copy_on_select), (false, true));
    }
}
