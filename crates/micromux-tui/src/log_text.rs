//! The text a log pane draws from log records, and the keys that find a record's lines in it
//! again after it is rebuilt.
//!
//! A selected service's pane and the pane of every service's logs both draw a [`LogText`], so
//! scrolling, search and mouse selection work the same in either.

use ansi_to_tui::IntoText as _;
use ratatui::text::{Line, Span, Text};

use crate::render::log_view::RenderedLineIndex;

/// A record's place in a log pane, which orders the records as the pane shows them and finds
/// the record's lines again after older records are dropped.
///
/// A single service's pane leaves `timestamp_unix_ms` and `service` at zero, so its records
/// order by sequence alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RecordKey {
    /// When micromux captured the record, in Unix milliseconds.
    pub timestamp_unix_ms: u64,
    /// The service's place in the roster, which orders records captured in the same
    /// millisecond.
    pub service: u32,
    /// The record's sequence number in its service's log.
    pub seq: u64,
}

impl RecordKey {
    /// The key of record `seq` in a single service's pane.
    pub(crate) fn of_seq(seq: u64) -> Self {
        Self {
            seq,
            ..Self::default()
        }
    }
}

/// Where a record starts in the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RecordStart {
    key: RecordKey,
    first_line: usize,
}

/// A line of the text by the record it comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RecordLine {
    pub record: RecordKey,
    /// The line within the record, which a live screen snapshot has several of.
    pub line: usize,
}

/// A rendered row of the text by the line it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowAnchor {
    pub record_line: RecordLine,
    /// The row within the line, which wrapping spreads over several.
    pub row_in_line: usize,
}

/// A record whose every line is led by a tag, such as the name of the service it comes from.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TaggedRecord<'a> {
    pub key: RecordKey,
    pub text: &'a str,
    pub tag: &'a [Span<'static>],
}

/// How the rows of the text are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RowLayout {
    wrap: bool,
    width: u16,
}

/// The rendered text of a log pane, with the rows its lines are drawn on and the records they
/// come from.
#[derive(Debug, Default)]
pub(crate) struct LogText {
    pub text: Text<'static>,
    /// Where each record starts, ordered by key and by line alike.
    record_starts: Vec<RecordStart>,
    pub line_index: RenderedLineIndex,
    /// The layout `line_index` describes, or `None` before the first build.
    layout: Option<RowLayout>,
    /// Counts the rebuilds of the text and its layout, so what was computed from them, such as
    /// search matches and scroll anchors, can tell when it is stale.
    pub generation: u64,
}

impl LogText {
    /// Replaces the text with `records`, ordered by key, laid out for `wrap` at `width` columns.
    ///
    /// The records are parsed as one text, so a color carries from one line to the next as it
    /// does in a terminal.
    pub(crate) fn rebuild<'a>(
        &mut self,
        records: impl IntoIterator<Item = (RecordKey, &'a str)> + Clone,
        wrap: bool,
        width: u16,
    ) {
        let joined = records
            .clone()
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("\n");
        self.text = parse_ansi(&joined);
        self.record_starts.clear();
        let mut next = 0;
        for (key, text) in records {
            self.record_starts.push(RecordStart {
                key,
                first_line: next,
            });
            next += 1 + text.matches('\n').count();
        }
        // Joining drops the line of a blank last record, which keeps a line of its own.
        if self.text.lines.len() < next {
            self.text.lines.resize(next, Line::default());
        }
        self.lay_out(wrap, width);
    }

    /// Replaces the text with `records`, ordered by key, each line led by its record's tag, laid
    /// out for `wrap` at `width` columns.
    ///
    /// Each record is parsed on its own, so a color one record leaves set never bleeds into the
    /// next, which may come from another service.
    pub(crate) fn rebuild_tagged<'a>(
        &mut self,
        records: impl IntoIterator<Item = TaggedRecord<'a>>,
        wrap: bool,
        width: u16,
    ) {
        self.text = Text::default();
        self.record_starts.clear();
        for TaggedRecord { key, text, tag } in records {
            self.record_starts.push(RecordStart {
                key,
                first_line: self.text.lines.len(),
            });
            let mut lines = parse_ansi(text).lines;
            if lines.is_empty() {
                lines.push(Line::default());
            }
            for mut line in lines {
                line.spans.splice(0..0, tag.iter().cloned());
                self.text.lines.push(line);
            }
        }
        self.lay_out(wrap, width);
    }

    /// Lays the text out for `wrap` at `width` columns, unless it is laid out so already.
    pub(crate) fn relayout(&mut self, wrap: bool, width: u16) {
        if self.layout != Some(RowLayout { wrap, width }) {
            self.lay_out(wrap, width);
        }
    }

    fn lay_out(&mut self, wrap: bool, width: u16) {
        self.line_index.rebuild(&self.text, wrap, width);
        self.layout = Some(RowLayout { wrap, width });
        self.generation = self.generation.wrapping_add(1);
    }

    /// The width the text's lines wrap at, or `None` when they do not wrap.
    pub(crate) fn wrap_width(&self) -> Option<u16> {
        self.layout
            .filter(|layout| layout.wrap)
            .map(|layout| layout.width)
    }

    /// The record line `line` of the text comes from, which stays with the line while older
    /// records are dropped, or `None` before any record was rendered.
    pub(crate) fn record_line(&self, line: usize) -> Option<RecordLine> {
        let record = self
            .record_starts
            .partition_point(|start| start.first_line <= line)
            .checked_sub(1)?;
        let start = self.record_starts.get(record)?;
        Some(RecordLine {
            record: start.key,
            line: line - start.first_line,
        })
    }

    /// The line of the text that `key` names, or, when its record was dropped, `Err` with the
    /// line now in its place.
    pub(crate) fn find_record_line(&self, key: RecordLine) -> Result<usize, usize> {
        let start_of = |record: usize| {
            self.record_starts
                .get(record)
                .map_or(self.text.lines.len(), |start| start.first_line)
        };
        match self
            .record_starts
            .binary_search_by_key(&key.record, |start| start.key)
        {
            Ok(record) => Ok(start_of(record) + key.line),
            Err(record) => Err(start_of(record)),
        }
    }

    /// What rendered `row` shows, by a key that stays with it while lines arrive and older ones
    /// are dropped, or `None` before any record was rendered.
    pub(crate) fn row_anchor(&self, row: usize) -> Option<RowAnchor> {
        let (line, first_row) = self.line_index.line_of_row(row)?;
        Some(RowAnchor {
            record_line: self.record_line(line)?,
            row_in_line: row - first_row,
        })
    }

    /// The rendered row `anchor` names in the current text: the same row of the same line,
    /// capped at the line's last row when it now wraps onto fewer, or the first row of the line
    /// in its place when its record was dropped.
    pub(crate) fn anchored_row(&self, anchor: RowAnchor) -> usize {
        let index = &self.line_index;
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
}

/// `text` with its ANSI colors parsed, or stripped of them when it cannot be parsed.
fn parse_ansi(text: &str) -> Text<'static> {
    text.into_text().unwrap_or_else(|err| {
        tracing::error!(
            ?err,
            input_bytes = text.len(),
            "failed to sanitize log buffer"
        );
        strip_ansi_escapes::strip_str(text).into()
    })
}
