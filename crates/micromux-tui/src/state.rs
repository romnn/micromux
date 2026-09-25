use micromux::LogLine;

use crate::json_log::LineFormat;

/// View state for one service. Domain state (execution, health, logs, healthchecks) lives in the
/// [`crate::SessionSource`]; this is the per-service render cache the TUI keeps.
#[derive(Debug)]
pub struct Service {
    pub snapshot: micromux::ServiceSnapshot,
    pub cached_lines: std::collections::VecDeque<(u64, String)>,
    pub cached_text: ratatui::text::Text<'static>,
    pub text_dirty: bool,
    pub cached_line_index: crate::render::log_view::RenderedLineIndex,
    pub cached_wrap: Option<(bool, u16)>,
    pub logs_dirty: bool,
    pub healthcheck_cached_text: ratatui::text::Text<'static>,
    pub healthcheck_cached_line_index: crate::render::log_view::RenderedLineIndex,
    pub healthcheck_cached_wrap: Option<(bool, u16)>,
    pub healthcheck_dirty: bool,
    /// Highest log sequence read into `cached_lines`, including records the format filtered out.
    pub log_cursor: u64,
    /// The format `cached_lines` was rendered with; a different format requires a full rebuild.
    pub cached_format: Option<LineFormat>,
    /// Level threshold picked in the viewer, taking precedence over the configured one.
    pub level_override: Option<crate::level::LevelOverride>,
    /// Timestamp visibility toggled in the viewer, taking precedence over the configured one.
    pub timestamps_override: Option<bool>,
    /// Field filtering toggled in the viewer, taking precedence over the configured one.
    pub filter_fields_override: Option<bool>,
}

impl Service {
    pub(crate) fn new(snapshot: micromux::ServiceSnapshot) -> Self {
        Self {
            snapshot,
            cached_lines: std::collections::VecDeque::new(),
            cached_text: ratatui::text::Text::default(),
            text_dirty: true,
            cached_line_index: crate::render::log_view::RenderedLineIndex::default(),
            cached_wrap: None,
            logs_dirty: true,
            healthcheck_cached_text: ratatui::text::Text::default(),
            healthcheck_cached_line_index: crate::render::log_view::RenderedLineIndex::default(),
            healthcheck_cached_wrap: None,
            healthcheck_dirty: true,
            log_cursor: 0,
            cached_format: None,
            level_override: None,
            timestamps_override: None,
            filter_fields_override: None,
        }
    }

    /// Drops the rendered lines when `format` differs from the one they were built with.
    pub(crate) fn use_format(&mut self, format: &LineFormat) {
        if self.cached_format.as_ref() == Some(format) {
            return;
        }
        // A new format changes which records show and how, so render the retained log again.
        self.cached_lines.clear();
        self.log_cursor = 0;
        self.cached_format = Some(format.clone());
        self.logs_dirty = true;
    }

    /// The sequence to read log records after.
    ///
    /// It re-reads the newest record seen, because the model replaces that record in place while
    /// a terminal frame is still being drawn.
    pub(crate) fn log_read_cursor(&self) -> u64 {
        self.log_cursor.saturating_sub(1)
    }

    /// Merges records read after [`Self::log_read_cursor`] into the rendered lines.
    ///
    /// `first_retained` is the model's oldest retained sequence, or `None` once the log is empty;
    /// lines older than it are evicted.
    pub(crate) fn apply_log_records(
        &mut self,
        first_retained: Option<u64>,
        records: &[LogLine],
        format: &LineFormat,
    ) {
        match first_retained {
            None => self.cached_lines.clear(),
            Some(first) => {
                while self
                    .cached_lines
                    .front()
                    .is_some_and(|(seq, _)| *seq < first)
                {
                    self.cached_lines.pop_front();
                }
            }
        }
        for record in records {
            self.log_cursor = self.log_cursor.max(record.seq);
            let formatted = crate::json_log::format_record(record, format);
            let replaces_last = self
                .cached_lines
                .back()
                .is_some_and(|(seq, _)| *seq == record.seq);
            match (formatted, replaces_last) {
                (Some(formatted), true) => {
                    if let Some((_, cached)) = self.cached_lines.back_mut() {
                        *cached = formatted;
                    }
                }
                (Some(formatted), false) => self.cached_lines.push_back((record.seq, formatted)),
                // The replacement now falls below the level threshold.
                (None, true) => {
                    self.cached_lines.pop_back();
                }
                (None, false) => {}
            }
        }
        self.text_dirty = true;
        self.logs_dirty = false;
    }

    /// Whether structured records lead with their timestamp: the viewer's toggle, else the config.
    pub(crate) fn shows_timestamps(&self) -> bool {
        self.timestamps_override
            .unwrap_or(self.snapshot.log_display.timestamps)
    }

    /// Whether the configured hidden fields are left out: the viewer's toggle, else the config.
    pub(crate) fn filters_fields(&self) -> bool {
        self.filter_fields_override
            .unwrap_or(self.snapshot.log_display.filter_fields)
    }

    /// The level threshold in effect: the viewer's pick, else the configured one.
    pub(crate) fn min_level(&self) -> Option<micromux::StructuredLogLevel> {
        self.level_override
            .map_or(self.snapshot.log_display.level, |level_override| {
                level_override.min_level
            })
    }

    /// The format the log pane renders this service with under the settings in effect.
    pub(crate) fn line_format(&self, pretty_json: bool) -> LineFormat {
        LineFormat {
            pretty_json,
            timestamps: self.shows_timestamps(),
            min_level: self.min_level(),
            hidden_fields: if self.filters_fields() {
                self.snapshot.log_display.hide_fields.clone()
            } else {
                Vec::new()
            },
        }
    }
}

#[derive(Debug)]
pub struct State {
    pub services: Vec<Service>,
    pub services_sidebar_width: u16,
    pub selected_service: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            services: Vec::new(),
            services_sidebar_width: crate::style::INITIAL_SIDEBAR_WIDTH,
            selected_service: 0,
        }
    }
}

impl State {
    #[must_use]
    pub fn new(services: Vec<Service>) -> Self {
        Self {
            services,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn current_service(&self) -> Option<&Service> {
        self.services.get(self.selected_service)
    }

    #[must_use]
    pub fn current_service_mut(&mut self) -> Option<&mut Service> {
        self.services.get_mut(self.selected_service)
    }

    /// Update the selection index.
    pub fn service_down(&mut self) {
        self.selected_service = self
            .selected_service
            .saturating_add(1)
            .min(self.services.len().saturating_sub(1));
    }

    pub fn service_up(&mut self) {
        self.selected_service = self.selected_service.saturating_sub(1);
    }

    pub fn resize_left(&mut self) {
        let minimum = self
            .services_sidebar_width
            .min(crate::style::MIN_SIDEBAR_WIDTH);
        self.services_sidebar_width = self.services_sidebar_width.saturating_sub(2).max(minimum);
    }

    pub fn resize_right(&mut self, max_width: u16) {
        if max_width < crate::style::MIN_SIDEBAR_WIDTH {
            self.services_sidebar_width =
                self.services_sidebar_width.saturating_add(2).min(max_width);
            return;
        }
        self.services_sidebar_width = self
            .services_sidebar_width
            .saturating_add(2)
            .clamp(crate::style::MIN_SIDEBAR_WIDTH, max_width);
    }

    pub fn clamp_sidebar(&mut self, max_width: u16) {
        self.services_sidebar_width = if max_width < crate::style::MIN_SIDEBAR_WIDTH {
            self.services_sidebar_width.min(max_width)
        } else {
            self.services_sidebar_width
                .clamp(crate::style::MIN_SIDEBAR_WIDTH, max_width)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::{Service, State};
    use crate::json_log::LineFormat;
    use micromux::{LogLine, StructuredLogLevel};
    use similar_asserts::assert_eq;

    fn service() -> Service {
        Service::new(micromux::ServiceSnapshot::initial(
            "svc".to_string(),
            "svc".to_string(),
            Vec::new(),
            None,
            micromux::RestartPolicy::Never,
            Vec::new(),
            None,
        ))
    }

    fn record(seq: u64, line: &str) -> LogLine {
        LogLine {
            seq,
            run_generation: 1,
            timestamp_unix_ms: 0,
            line: line.to_string(),
        }
    }

    fn cached(service: &Service) -> Vec<(u64, &str)> {
        service
            .cached_lines
            .iter()
            .map(|(seq, line)| (*seq, line.as_str()))
            .collect()
    }

    #[test]
    fn filtered_records_advance_the_cursor_and_in_place_replacements_follow_the_filter() {
        let format = LineFormat {
            pretty_json: false,
            timestamps: false,
            min_level: Some(StructuredLogLevel::Info),
            hidden_fields: Vec::new(),
        };
        let mut service = service();
        service.use_format(&format);

        service.apply_log_records(
            Some(1),
            &[
                record(1, r#"{"level":"info","msg":"kept"}"#),
                record(2, r#"{"level":"debug","msg":"hidden"}"#),
            ],
            &format,
        );

        // A hidden record still moves the cursor, so the next read does not start before it.
        assert_eq!(
            cached(&service),
            vec![(1, r#"{"level":"info","msg":"kept"}"#)]
        );
        assert_eq!(service.log_read_cursor(), 1);

        // The model replaced the newest record in place; the rewrite now passes the filter.
        service.apply_log_records(
            Some(1),
            &[record(2, r#"{"level":"warn","msg":"now shown"}"#)],
            &format,
        );
        assert_eq!(
            cached(&service),
            vec![
                (1, r#"{"level":"info","msg":"kept"}"#),
                (2, r#"{"level":"warn","msg":"now shown"}"#),
            ]
        );

        // A later rewrite of that record that falls below the threshold removes it again.
        service.apply_log_records(
            Some(1),
            &[record(2, r#"{"level":"trace","msg":"gone"}"#)],
            &format,
        );
        assert_eq!(
            cached(&service),
            vec![(1, r#"{"level":"info","msg":"kept"}"#)]
        );
    }

    #[test]
    fn a_new_format_discards_the_rendered_lines_and_rereads_from_the_start() {
        let all = LineFormat {
            pretty_json: false,
            timestamps: false,
            min_level: None,
            hidden_fields: Vec::new(),
        };
        let mut service = service();
        service.use_format(&all);
        service.apply_log_records(Some(1), &[record(1, "line")], &all);

        // Reusing the same format keeps the cache.
        service.use_format(&all);
        assert!(!service.logs_dirty);
        assert_eq!(cached(&service), vec![(1, "line")]);

        // Any change requests a full re-read.
        service.use_format(&LineFormat {
            timestamps: true,
            ..all
        });
        assert!(service.logs_dirty);
        assert!(service.cached_lines.is_empty());
        assert_eq!(service.log_read_cursor(), 0);
    }

    #[test]
    fn sidebar_never_exceeds_a_narrow_terminal() {
        let mut state = State::default();

        state.clamp_sidebar(5);
        assert_eq!(state.services_sidebar_width, 5);
        state.resize_right(5);
        assert_eq!(state.services_sidebar_width, 5);
        state.resize_left();
        assert_eq!(state.services_sidebar_width, 5);
        state.services_sidebar_width = 3;
        state.resize_right(5);
        assert_eq!(state.services_sidebar_width, 5);
    }
}
