//! The user config: personal settings that apply to every project.
//!
//! It lives at [`user_config_path`] and uses the same keys as a project's `micromux.yaml`, so a
//! setting reads the same in both files.
//! A project's own value takes precedence over the user's.

use std::path::PathBuf;

use codespan_reporting::diagnostic::Diagnostic;
use yaml_spanned::{Spanned, Value};

use super::v1::expect_mapping;
use super::{ConfigError, parse_optional};

/// Name of the user config file inside micromux's config directory.
pub const USER_CONFIG_FILE_NAME: &str = "config.yaml";

/// Settings from the user config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserConfig {
    /// `ui.mouse`: whether the TUI captures the mouse, or `None` when the file does not say.
    pub mouse: Option<bool>,
    /// `ui.copy_on_select`: whether finishing a mouse selection copies it, or `None` when the
    /// file does not say.
    pub copy_on_select: Option<bool>,
}

/// Where the user config lives: `config.yaml` in the platform's config directory for micromux,
/// such as `~/.config/micromux/config.yaml` on Linux.
///
/// Returns `None` when the platform has no home directory to put it in.
#[must_use]
pub fn user_config_path() -> Option<PathBuf> {
    crate::project_dir().map(|directories| directories.config_dir().join(USER_CONFIG_FILE_NAME))
}

/// Parses the text of a user config.
///
/// An empty file is an empty config.
/// Unknown keys are warnings in `diagnostics`, so a file written for a newer micromux still
/// loads.
///
/// # Errors
///
/// Returns an error when the text is not YAML, when its root or `ui` is not a mapping, or when
/// a known key has a value of the wrong type.
pub fn parse_user_config<F: Copy>(
    raw: &str,
    file_id: F,
    diagnostics: &mut Vec<Diagnostic<F>>,
) -> Result<UserConfig, ConfigError> {
    let root = yaml_spanned::from_str(raw).map_err(ConfigError::Yaml)?;
    if matches!(root.inner, Value::Null) {
        return Ok(UserConfig::default());
    }
    let (_span, mapping) = expect_mapping(&root, Some("the user config must be a mapping"))?;
    super::v1::warn_unknown_keys(mapping, &["ui"], "user config", file_id, false, diagnostics);
    let Some(ui) = mapping.get("ui") else {
        return Ok(UserConfig::default());
    };
    let (_span, ui) = expect_mapping(ui, Some("ui config must be a mapping"))?;
    super::v1::warn_unknown_keys(
        ui,
        &["mouse", "copy_on_select"],
        "ui",
        file_id,
        false,
        diagnostics,
    );
    let mouse = parse_optional::<bool>(ui.get("mouse"))?.map(Spanned::into_inner);
    let copy_on_select = parse_optional::<bool>(ui.get("copy_on_select"))?.map(Spanned::into_inner);
    Ok(UserConfig {
        mouse,
        copy_on_select,
    })
}

#[cfg(test)]
mod tests {
    use super::{UserConfig, parse_user_config};
    use color_eyre::eyre;
    use indoc::indoc;
    use similar_asserts::assert_eq;

    #[test]
    fn reads_the_mouse_settings() -> eyre::Result<()> {
        let mut diagnostics = Vec::new();
        let config = parse_user_config(
            "ui:\n  mouse: false\n  copy_on_select: true\n",
            0usize,
            &mut diagnostics,
        )?;
        assert_eq!(
            config,
            UserConfig {
                mouse: Some(false),
                copy_on_select: Some(true),
            }
        );
        assert_eq!(diagnostics, Vec::new());
        Ok(())
    }

    #[test]
    fn empty_files_and_sections_leave_every_setting_unset() -> eyre::Result<()> {
        let mut diagnostics = Vec::new();
        for raw in ["", "# nothing yet\n", "ui: {}\n"] {
            assert_eq!(
                parse_user_config(raw, 0usize, &mut diagnostics)?,
                UserConfig::default(),
                "{raw:?}"
            );
        }
        assert_eq!(diagnostics, Vec::new());
        Ok(())
    }

    #[test]
    fn unknown_keys_warn_without_failing() -> eyre::Result<()> {
        let raw = indoc! {"
            theme: dark
            ui:
              mouse: true
              colors: many
        "};
        let mut diagnostics = Vec::new();
        let config = parse_user_config(raw, 0usize, &mut diagnostics)?;

        // The known key still applies, and each unknown one gets a warning of its own.
        assert_eq!(
            config,
            UserConfig {
                mouse: Some(true),
                copy_on_select: None,
            }
        );
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            messages,
            vec![
                "unknown user config field `theme`",
                "unknown ui field `colors`"
            ]
        );
        Ok(())
    }

    #[test]
    fn a_mouse_setting_that_is_not_a_bool_is_an_error() {
        let mut diagnostics = Vec::new();
        assert!(parse_user_config("ui:\n  mouse: sometimes\n", 0usize, &mut diagnostics).is_err());
        assert!(parse_user_config("- ui\n", 0usize, &mut diagnostics).is_err());
    }
}
