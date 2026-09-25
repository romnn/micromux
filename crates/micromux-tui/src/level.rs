//! The structured-log level threshold an operator picks per service in the viewer.

use micromux::{ServiceID, StructuredLogLevel};

/// Thresholds offered by the level picker, from most to least verbose.
///
/// There is no separate trace entry because a trace threshold hides nothing that "all" shows:
/// lines without a recognizable level always pass.
pub(crate) const LEVEL_CHOICES: [Option<StructuredLogLevel>; 5] = [
    None,
    Some(StructuredLogLevel::Debug),
    Some(StructuredLogLevel::Info),
    Some(StructuredLogLevel::Warn),
    Some(StructuredLogLevel::Error),
];

/// Returns the uppercase label for a threshold, `ALL` when every level shows.
pub(crate) fn threshold_label(min_level: Option<StructuredLogLevel>) -> String {
    min_level.map_or_else(
        || "ALL".to_string(),
        |level| level.canonical().to_ascii_uppercase(),
    )
}

/// A threshold picked in the viewer, replacing the configured one for a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LevelOverride {
    /// Least severe structured-log level shown, or `None` for every level.
    pub min_level: Option<StructuredLogLevel>,
}

/// The open level-picker popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LevelPicker {
    /// Service whose threshold the picker changes, fixed when the popup opens so a roster change
    /// cannot redirect the choice to another service.
    pub service_id: ServiceID,
    /// Highlighted index into [`LEVEL_CHOICES`].
    pub cursor: usize,
}

impl LevelPicker {
    /// Opens the picker with the service's current threshold highlighted.
    pub(crate) fn open(service_id: ServiceID, current: Option<StructuredLogLevel>) -> Self {
        let cursor = LEVEL_CHOICES
            .iter()
            .position(|choice| *choice == current)
            .unwrap_or_default();
        Self { service_id, cursor }
    }

    pub(crate) fn up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub(crate) fn down(&mut self) {
        self.cursor = self
            .cursor
            .saturating_add(1)
            .min(LEVEL_CHOICES.len().saturating_sub(1));
    }

    /// The highlighted threshold.
    pub(crate) fn selected(&self) -> Option<StructuredLogLevel> {
        LEVEL_CHOICES.get(self.cursor).copied().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::{LEVEL_CHOICES, LevelPicker, threshold_label};
    use micromux::StructuredLogLevel;
    use similar_asserts::assert_eq;

    #[test]
    fn picker_opens_on_the_current_threshold_and_stays_in_bounds() {
        let mut picker = LevelPicker::open("svc".to_string(), Some(StructuredLogLevel::Info));
        assert_eq!(picker.selected(), Some(StructuredLogLevel::Info));

        // Moving past either end clamps instead of wrapping.
        for _ in 0..LEVEL_CHOICES.len() {
            picker.down();
        }
        assert_eq!(picker.selected(), Some(StructuredLogLevel::Error));
        for _ in 0..LEVEL_CHOICES.len() {
            picker.up();
        }
        assert_eq!(picker.selected(), None);

        // A configured threshold the picker does not offer falls back to "all".
        let fatal = LevelPicker::open("svc".to_string(), Some(StructuredLogLevel::Fatal));
        assert_eq!(fatal.cursor, 0);
    }

    #[test]
    fn threshold_labels_are_uppercase_level_names() {
        assert_eq!(threshold_label(None), "ALL");
        assert_eq!(threshold_label(Some(StructuredLogLevel::Warn)), "WARN");
    }
}
