use micromux::LogLine;

use crate::json_log::LineFormat;
use crate::log_text::{LogText, RecordKey};

/// View state for one service. Domain state (execution, health, logs, healthchecks) lives in the
/// [`crate::SessionSource`]; this is the per-service render cache the TUI keeps.
#[derive(Debug)]
pub struct Service {
    pub snapshot: micromux::ServiceSnapshot,
    pub cached_lines: std::collections::VecDeque<CachedLine>,
    /// The log pane's text for this service.
    ///
    /// It describes the text as rendered, so it stays right while records read since then wait
    /// in `cached_lines` for the next rebuild.
    pub(crate) log: LogText,
    pub text_dirty: bool,
    pub logs_dirty: bool,
    pub healthcheck_cached_text: ratatui::text::Text<'static>,
    pub healthcheck_cached_line_index: crate::render::log_view::RenderedLineIndex,
    pub healthcheck_cached_wrap: Option<(bool, u16)>,
    pub healthcheck_dirty: bool,
    /// Highest log sequence read into `cached_lines`, including records the format filtered out.
    pub log_cursor: u64,
    /// Counts the changes to `cached_lines` other than appending lines and dropping the oldest,
    /// so a view built from them can tell when it must be built again.
    pub(crate) rewrites: u64,
    /// The format `cached_lines` was rendered with; a different format requires a full rebuild.
    pub cached_format: Option<LineFormat>,
    /// Level threshold picked in the viewer, taking precedence over the configured one.
    pub level_override: Option<crate::level::LevelOverride>,
    /// Timestamp visibility toggled in the viewer, taking precedence over the configured one.
    pub timestamps_override: Option<bool>,
    /// Field filtering toggled in the viewer, taking precedence over the configured one.
    pub filter_fields_override: Option<bool>,
}

/// A log record as the log pane renders it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedLine {
    /// The record's sequence number.
    pub seq: u64,
    /// When micromux first captured the record, in Unix milliseconds.
    ///
    /// A live screen snapshot the model replaces in place keeps the time of its first frame, so
    /// it stays where it started among every service's logs.
    pub timestamp_unix_ms: u64,
    /// The record formatted for the pane, possibly several lines of a live screen snapshot.
    pub text: String,
}

impl Service {
    pub(crate) fn new(snapshot: micromux::ServiceSnapshot) -> Self {
        Self {
            snapshot,
            cached_lines: std::collections::VecDeque::new(),
            log: LogText::default(),
            text_dirty: true,
            logs_dirty: true,
            healthcheck_cached_text: ratatui::text::Text::default(),
            healthcheck_cached_line_index: crate::render::log_view::RenderedLineIndex::default(),
            healthcheck_cached_wrap: None,
            healthcheck_dirty: true,
            log_cursor: 0,
            rewrites: 0,
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
        self.rewrites += 1;
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

    /// Brings `cached_lines` up to date with `source` under the format the settings in effect
    /// pick, and returns whether they changed.
    pub(crate) fn read_logs(&mut self, source: &crate::SessionSource, pretty_json: bool) -> bool {
        let format = self.line_format(pretty_json);
        self.use_format(&format);
        if !self.logs_dirty {
            return false;
        }
        let (first_retained, records) =
            source.logs_since(&self.snapshot.id, self.log_read_cursor());
        self.apply_log_records(first_retained, &records, &format);
        true
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
            None => {
                if !self.cached_lines.is_empty() {
                    self.cached_lines.clear();
                    self.rewrites += 1;
                }
            }
            Some(first) => {
                while self
                    .cached_lines
                    .front()
                    .is_some_and(|cached| cached.seq < first)
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
                .is_some_and(|cached| cached.seq == record.seq);
            match (formatted, replaces_last) {
                // Each read takes the newest record again, which mostly comes back unchanged.
                (Some(text), true) => {
                    if let Some(cached) = self.cached_lines.back_mut()
                        && cached.text != text
                    {
                        cached.text = text;
                        self.rewrites += 1;
                    }
                }
                (Some(text), false) => self.cached_lines.push_back(CachedLine {
                    seq: record.seq,
                    timestamp_unix_ms: record.timestamp_unix_ms,
                    text,
                }),
                // The replacement now falls below the level threshold.
                (None, true) => {
                    self.cached_lines.pop_back();
                    self.rewrites += 1;
                }
                (None, false) => {}
            }
        }
        self.text_dirty = true;
        self.logs_dirty = false;
    }

    /// Renders `cached_lines` into the pane's text and indexes its rows for `wrap` at
    /// `wrap_width` columns, unless that is done already.
    pub(crate) fn render_text(&mut self, wrap: bool, wrap_width: u16) {
        if !self.text_dirty {
            self.log.relayout(wrap, wrap_width);
            return;
        }
        self.log.rebuild(
            self.cached_lines
                .iter()
                .map(|cached| (RecordKey::of_seq(cached.seq), cached.text.as_str())),
            wrap,
            wrap_width,
        );
        self.text_dirty = false;
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
    /// Every service's logs interleaved, while the log pane shows them instead of the selected
    /// service's.
    pub(crate) all_logs: Option<crate::all_logs::AllLogs>,
}

/// Whose logs the log pane shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShownLogs {
    /// The service with this ID.
    Service(String),
    /// Every service, interleaved.
    All,
}

/// The logs the log pane shows and their text.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ShownLog<'a> {
    pub service: Option<&'a Service>,
    pub log: &'a LogText,
}

impl ShownLog<'_> {
    /// Whose logs these are.
    pub(crate) fn shows(&self) -> ShownLogs {
        self.service.map_or(ShownLogs::All, |service| {
            ShownLogs::Service(service.snapshot.id.clone())
        })
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            services: Vec::new(),
            services_sidebar_width: crate::style::INITIAL_SIDEBAR_WIDTH,
            selected_service: 0,
            all_logs: None,
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

    /// The logs the log pane shows: every service's while they are shown, else the selected
    /// service's, or `None` without a service to show.
    pub(crate) fn shown_log(&self) -> Option<ShownLog<'_>> {
        match &self.all_logs {
            Some(all_logs) => Some(ShownLog {
                service: None,
                log: &all_logs.log,
            }),
            None => self.current_service().map(|service| ShownLog {
                service: Some(service),
                log: &service.log,
            }),
        }
    }

    /// Reads new log records and renders the text the log pane shows for `wrap` at `wrap_width`
    /// columns.
    pub(crate) fn update_shown_log(
        &mut self,
        source: &crate::SessionSource,
        pretty_json: bool,
        wrap: bool,
        wrap_width: u16,
    ) {
        match &mut self.all_logs {
            Some(all_logs) => {
                for service in &mut self.services {
                    if service.read_logs(source, pretty_json) {
                        all_logs.dirty = true;
                    }
                }
                all_logs.render_text(&self.services, wrap, wrap_width);
            }
            None => {
                if let Some(service) = self.services.get_mut(self.selected_service) {
                    service.read_logs(source, pretty_json);
                    service.render_text(wrap, wrap_width);
                }
            }
        }
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
            .map(|cached| (cached.seq, cached.text.as_str()))
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
