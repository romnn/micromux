//! The TUI's mouse settings: what the mouse may do, and whether finishing a selection copies it.
//!
//! For each setting, and for each part of `ui.mouse` on its own, the first layer that says wins:
//!
//! 1. The command-line flag, or its environment variable.
//! 2. The project's `ui` section in `micromux.yaml`.
//! 3. The user's `ui` section in the user config.
//! 4. The default.

use std::path::Path;

use crate::{DiagnosticsPrinter, Error};

/// Each part of the mouse when no setting says otherwise.
///
/// Every part is on because the TUI selects and copies text inside one pane itself, so
/// capturing costs no text selection, and because the wheel and clicks are how most people
/// expect to scroll and pick panes.
const DEFAULT_MOUSE_PART: bool = true;

/// Copy-on-select when no setting says otherwise.
///
/// It is off because a selection that overwrites the clipboard by itself surprises people who
/// keep something there, so copying waits for `y` or Enter.
const DEFAULT_COPY_ON_SELECT: bool = false;

/// One setting from each layer, or `None` where that layer is silent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Layers {
    pub(crate) mouse: micromux::MouseConfig,
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
        self.mouse.is_complete() && self.copy_on_select.is_some()
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
    let part = |part: Option<bool>| part.unwrap_or(DEFAULT_MOUSE_PART);
    micromux_tui::MouseOptions {
        select: part(settings.mouse.select),
        navigate: part(settings.mouse.navigate),
        scroll: part(settings.mouse.scroll),
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
        let user = crate::user_config::load(printer).await?;
        Layers {
            mouse: user.mouse,
            copy_on_select: user.copy_on_select,
        }
    };
    Ok(resolve(flags, project, user))
}

/// The `ui` section of the project config at `config_path`, or `None` when it cannot be read.
///
/// An attached TUI reads only that section: the session it attaches to already loaded and
/// reported on the config, so the rest is not this client's to judge.
pub(crate) async fn project_ui_config(config_path: &Path) -> Option<micromux::UiConfig> {
    let parsed = match micromux::read_config_file_async(config_path).await {
        Ok(raw) => micromux::parse_ui_section(&raw).map_err(|err| err.to_string()),
        Err(err) => Err(err.to_string()),
    };
    match parsed {
        Ok(ui) => Some(ui),
        Err(err) => {
            // Shown once the TUI gives the terminal back, and kept in the log.
            eprintln!(
                "warning: ignoring the ui settings of {}: {err}",
                config_path.display()
            );
            tracing::warn!(err, path = %config_path.display(), "ignoring the project's ui settings");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Layers, resolve};
    use micromux::MouseConfig;
    use similar_asserts::assert_eq;

    fn mouse(mouse: MouseConfig) -> Layers {
        Layers {
            mouse,
            copy_on_select: None,
        }
    }

    /// The parts of the mouse that `settings` turns on, as select, navigate and scroll.
    fn parts(settings: micromux_tui::MouseOptions) -> (bool, bool, bool) {
        (settings.select, settings.navigate, settings.scroll)
    }

    #[test]
    fn the_flag_beats_the_project_which_beats_the_user() {
        // Each layer decides only when every one above it is silent.
        let capture = |flags: Option<bool>, project: Option<bool>, user: Option<bool>| {
            let layer = |setting: Option<bool>| {
                mouse(setting.map_or_else(MouseConfig::default, MouseConfig::all))
            };
            let settings = resolve(layer(flags), layer(project), layer(user));
            match parts(settings) {
                (true, true, true) => true,
                (false, false, false) => false,
                other => panic!("a bool sets every part alike: {other:?}"),
            }
        };
        assert!(!capture(Some(false), Some(true), Some(true)));
        assert!(capture(Some(true), Some(false), Some(false)));
        assert!(!capture(None, Some(false), Some(true)));
        assert!(capture(None, Some(true), Some(false)));
        assert!(!capture(None, None, Some(false)));

        // With no setting at all, the mouse may do everything.
        assert!(capture(None, None, None));
    }

    /// Each part of the mouse comes from the first layer that sets it, so a partial mapping in
    /// one layer leaves the other parts to the layers below, and a part no layer sets is on.
    #[test]
    fn each_mouse_part_resolves_on_its_own() {
        let user = mouse(MouseConfig {
            select: Some(true),
            navigate: Some(false),
            scroll: None,
        });

        // The user's parts apply, and the scroll they leave open is on.
        let settings = resolve(Layers::default(), Layers::default(), user);
        assert_eq!(parts(settings), (true, false, true));

        // A project that turns navigation back on keeps the user's other parts.
        let project = mouse(MouseConfig {
            navigate: Some(true),
            ..MouseConfig::default()
        });
        assert_eq!(
            parts(resolve(Layers::default(), project, user)),
            (true, true, true)
        );

        // A project's `mouse: false` sets every part, so none of the user's applies.
        let project = mouse(MouseConfig::all(false));
        assert_eq!(
            parts(resolve(Layers::default(), project, user)),
            (false, false, false)
        );

        // Over a user's `mouse: false`, a project's mapping turns on only the parts it names.
        let project = mouse(MouseConfig {
            select: Some(true),
            ..MouseConfig::default()
        });
        let user = mouse(MouseConfig::all(false));
        assert_eq!(
            parts(resolve(Layers::default(), project, user)),
            (true, false, false)
        );

        // The flag sets every part, over any mapping.
        let flags = mouse(MouseConfig::all(true));
        assert_eq!(parts(resolve(flags, project, user)), (true, true, true));
    }

    #[test]
    fn copy_on_select_is_off_unless_a_layer_turns_it_on() {
        let copy_on_select = |flags, project, user| {
            let layer = |copy_on_select| Layers {
                mouse: MouseConfig::default(),
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
            mouse(MouseConfig::all(false)),
            Layers::default(),
            Layers {
                mouse: MouseConfig::all(true),
                copy_on_select: Some(true),
            },
        );
        assert_eq!(
            (parts(settings), settings.copy_on_select),
            ((false, false, false), true)
        );
    }
}
