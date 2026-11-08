use ansi_to_tui::IntoText as _;
use micromux::LogLine;

use crate::json_log::LineFormat;

/// View state for one service. Domain state (execution, health, logs, healthchecks) lives in the
/// [`crate::SessionSource`]; this is the per-service render cache the TUI keeps.
#[derive(Debug)]
pub struct Service {
    pub snapshot: micromux::ServiceSnapshot,
    pub cached_lines: std::collections::VecDeque<(u64, String)>,
    /// Where each record rendered into `cached_text` starts; a record of a live screen snapshot
    /// spans several lines.
    ///
    /// It describes the text as rendered, so it stays right while records read since then wait
    /// in `cached_lines` for the next rebuild.
    pub record_starts: Vec<RecordStart>,
    pub cached_text: ratatui::text::Text<'static>,
    pub text_dirty: bool,
    /// Counts the rebuilds of `cached_text`, so what was computed from it, such as search
    /// matches, can tell when it is stale.
    pub text_generation: u64,
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
            record_starts: Vec::new(),
            cached_text: ratatui::text::Text::default(),
            text_dirty: true,
            text_generation: 0,
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

    /// Renders `cached_lines` into `cached_text` and indexes its rows for `wrap` at `wrap_width`
    /// columns, unless that is done already.
    pub(crate) fn render_text(&mut self, wrap: bool, wrap_width: u16) {
        if !self.text_dirty && self.cached_wrap == Some((wrap, wrap_width)) {
            return;
        }
        let joined = self
            .cached_lines
            .iter()
            .map(|(_, line)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        self.cached_text = joined.as_str().into_text().unwrap_or_else(|err| {
            let escaped = strip_ansi_escapes::strip_str(&joined);
            tracing::error!(
                ?err,
                input_bytes = joined.len(),
                "failed to sanitize log buffer"
            );
            escaped.into()
        });
        // A selection finds its lines by the records they came from.
        self.record_starts.clear();
        let mut next = 0;
        for (seq, line) in &self.cached_lines {
            self.record_starts.push(RecordStart {
                seq: *seq,
                first_line: next,
            });
            next += 1 + line.matches('\n').count();
        }
        // Joining drops the line of a blank last record, which keeps a line of its own.
        if self.cached_text.lines.len() < next {
            self.cached_text
                .lines
                .resize(next, ratatui::text::Line::default());
        }
        self.cached_line_index
            .rebuild(&self.cached_text, wrap, wrap_width);
        self.cached_wrap = Some((wrap, wrap_width));
        self.text_dirty = false;
        self.text_generation = self.text_generation.wrapping_add(1);
    }

    /// The record line `line` of `cached_text` comes from, which stays with the line while older
    /// records are dropped, or `None` before any record was rendered.
    pub(crate) fn record_line(&self, line: usize) -> Option<RecordLine> {
        let record = self
            .record_starts
            .partition_point(|start| start.first_line <= line)
            .checked_sub(1)?;
        let start = self.record_starts.get(record)?;
        Some(RecordLine {
            seq: start.seq,
            line: line - start.first_line,
        })
    }

    /// The line of `cached_text` that `key` names, or, when its record was dropped, `Err` with
    /// the line now in its place.
    pub(crate) fn find_record_line(&self, key: RecordLine) -> Result<usize, usize> {
        let start_of = |record: usize| {
            self.record_starts
                .get(record)
                .map_or(self.cached_text.lines.len(), |start| start.first_line)
        };
        match self
            .record_starts
            .binary_search_by_key(&key.seq, |start| start.seq)
        {
            Ok(record) => Ok(start_of(record) + key.line),
            Err(record) => Err(start_of(record)),
        }
    }

    /// What rendered `row` of the log pane shows, by a key that stays with it while lines
    /// arrive and older ones are dropped, or `None` before any record was rendered.
    pub(crate) fn row_anchor(&self, row: usize) -> Option<RowAnchor> {
        let (line, first_row) = self.cached_line_index.line_of_row(row)?;
        Some(RowAnchor {
            record_line: self.record_line(line)?,
            row_in_line: row - first_row,
        })
    }

    /// The rendered row `anchor` names in the current text: the same row of the same line,
    /// capped at the line's last row when it now wraps onto fewer, or the first row of the line
    /// in its place when its record was dropped.
    pub(crate) fn anchored_row(&self, anchor: RowAnchor) -> usize {
        let index = &self.cached_line_index;
        match self.find_record_line(anchor.record_line) {
            Ok(line) => {
                let Some(first_row) = index.line_start(line) else {
                    return index.total_lines();
                };
                let next_row = index.line_start(line + 1).unwrap_or(index.total_lines());
                let last_row = next_row.saturating_sub(1).max(first_row);
                (first_row + anchor.row_in_line).min(last_row)
            }
            Err(line) => index.line_start(line).unwrap_or(index.total_lines()),
        }
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

/// Where a record starts in the log pane's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordStart {
    /// The record's sequence number.
    pub seq: u64,
    pub first_line: usize,
}

/// A line of the log pane's text by the record it comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RecordLine {
    /// The record's sequence number.
    pub seq: u64,
    /// The line within the record, which a live screen snapshot has several of.
    pub line: usize,
}

/// A rendered row of the log pane by the line it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowAnchor {
    pub record_line: RecordLine,
    /// The row within the line, which wrapping spreads over several.
    pub row_in_line: usize,
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
