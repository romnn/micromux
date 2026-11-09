//! The text a log pane draws from log records, and the keys that find a record's lines in it
//! again after it changes.
//!
//! A selected service's pane and the pane of every service's logs both draw a [`LogText`], so
//! scrolling, search and mouse selection work the same in either.

use std::collections::HashMap;

use ansi_to_tui::IntoText as _;
use ratatui::text::{Line, Span, Text};

use crate::render::log_view::RenderedLineIndex;

/// Which record a line comes from, which stays the same however the records around it change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct RecordId {
    /// The service the record comes from: zero in a single service's pane, and a number that
    /// stays with the service's ID while every service's logs show.
    pub service: u32,
    /// The record's sequence number in its service's log.
    pub seq: u64,
}

/// A record of a log pane: which one it is, and when micromux first captured it.
///
/// The capture time places a record that was dropped, so what pointed at it moves to the record
/// now in its place.
/// A single service's pane leaves it at zero, which orders its records by sequence alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RecordKey {
    pub id: RecordId,
    /// When micromux first captured the record, in Unix milliseconds.
    pub timestamp_unix_ms: u64,
}

impl RecordKey {
    /// The key of record `seq` in a single service's pane.
    pub(crate) fn of_seq(seq: u64) -> Self {
        Self {
            id: RecordId { service: 0, seq },
            timestamp_unix_ms: 0,
        }
    }
}

/// Where a record starts in the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RecordStart {
    key: RecordKey,
    /// Counts up through the records in the order of the text from its last full rebuild, so
    /// it finds the record while records before it are cut out.
    ordinal: u64,
    first_line: usize,
}

/// A line of the text by the record it comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// How the text changed in its last change, when lines were only cut out and appended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Edit {
    /// The generation the text had before.
    pub from_generation: u64,
    /// The lines cut out, numbered as they were before, in order and apart.
    pub removed_lines: Vec<std::ops::Range<usize>>,
    /// The first line appended after the lines that were kept, or the line count when none was.
    pub first_new_line: usize,
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
    /// Where each record starts, in the order of the text, and so by ordinal.
    record_starts: Vec<RecordStart>,
    /// The ordinal of each record.
    ordinals: HashMap<RecordId, u64>,
    next_ordinal: u64,
    pub line_index: RenderedLineIndex,
    /// The layout `line_index` describes, or `None` before the first build.
    layout: Option<RowLayout>,
    /// Counts the changes to the text and its layout, so what was computed from them, such as
    /// search matches and scroll anchors, can tell when it is stale.
    pub generation: u64,
    /// How the last change edited the text, or `None` when it rebuilt it whole.
    pub last_edit: Option<Edit>,
}

impl LogText {
    /// Replaces the text with `records`, laid out for `wrap` at `width` columns.
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
        self.clear_records();
        let mut next = 0;
        for (key, text) in records {
            self.push_record(key, next);
            next += 1 + text.matches('\n').count();
        }
        // Joining drops the line of a blank last record, which keeps a line of its own.
        if self.text.lines.len() < next {
            self.text.lines.resize(next, Line::default());
        }
        self.lay_out(wrap, width);
    }

    /// Replaces the text with `records`, each line led by its record's tag, laid out for `wrap`
    /// at `width` columns.
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
        self.clear_records();
        self.append_tagged(records);
        self.lay_out(wrap, width);
    }

    /// Cuts out the records `keep` rejects and appends `records`, each line led by its record's
    /// tag, laying out only the new lines.
    ///
    /// The text must be laid out already; the change is recorded in [`Self::last_edit`].
    pub(crate) fn edit_tagged<'a>(
        &mut self,
        keep: impl Fn(RecordKey) -> bool,
        records: impl IntoIterator<Item = TaggedRecord<'a>>,
    ) {
        let Some(RowLayout { wrap, width }) = self.layout else {
            return;
        };
        let removed_lines = self.cut_out(keep);
        let first_new_line = self.text.lines.len();
        self.append_tagged(records);
        self.line_index.extend(
            self.text.lines.get(first_new_line..).unwrap_or_default(),
            wrap,
            width,
        );
        let from_generation = self.generation;
        self.generation = self.generation.wrapping_add(1);
        self.last_edit = Some(Edit {
            from_generation,
            removed_lines,
            first_new_line,
        });
    }

    /// Cuts out the records `keep` rejects, with their lines and rows, and returns the lines cut
    /// out, numbered as they were.
    fn cut_out(&mut self, keep: impl Fn(RecordKey) -> bool) -> Vec<std::ops::Range<usize>> {
        let line_count = self.text.lines.len();
        let mut removed_lines: Vec<std::ops::Range<usize>> = Vec::new();
        for (index, start) in self.record_starts.iter().enumerate() {
            if keep(start.key) {
                continue;
            }
            let end = self
                .record_starts
                .get(index + 1)
                .map_or(line_count, |next| next.first_line);
            match removed_lines.last_mut() {
                Some(last) if last.end == start.first_line => last.end = end,
                _ => removed_lines.push(start.first_line..end),
            }
        }
        if removed_lines.is_empty() {
            return removed_lines;
        }

        // Records
        let ordinals = &mut self.ordinals;
        self.record_starts.retain(|start| {
            let kept = keep(start.key);
            if !kept {
                ordinals.remove(&start.key.id);
            }
            kept
        });
        let mut places = RemovedLines::new(&removed_lines);
        for start in &mut self.record_starts {
            start.first_line = places.place(start.first_line).unwrap_or(start.first_line);
        }

        // Lines and their rows
        let mut places = RemovedLines::new(&removed_lines);
        let mut line = 0;
        self.text.lines.retain(|_| {
            let kept = places.place(line).is_some();
            line += 1;
            kept
        });
        self.line_index.remove(&removed_lines);
        removed_lines
    }

    /// Lays the text out for `wrap` at `width` columns, unless it is laid out so already.
    pub(crate) fn relayout(&mut self, wrap: bool, width: u16) {
        if self.layout == Some(RowLayout { wrap, width }) {
            return;
        }
        let from_generation = self.generation;
        self.lay_out(wrap, width);
        // The lines themselves stay as they are.
        self.last_edit = Some(Edit {
            from_generation,
            removed_lines: Vec::new(),
            first_new_line: self.text.lines.len(),
        });
    }

    fn clear_records(&mut self) {
        self.record_starts.clear();
        self.ordinals.clear();
        self.next_ordinal = 0;
    }

    fn push_record(&mut self, key: RecordKey, first_line: usize) {
        let ordinal = self.next_ordinal;
        self.next_ordinal += 1;
        self.ordinals.insert(key.id, ordinal);
        self.record_starts.push(RecordStart {
            key,
            ordinal,
            first_line,
        });
    }

    fn append_tagged<'a>(&mut self, records: impl IntoIterator<Item = TaggedRecord<'a>>) {
        for TaggedRecord { key, text, tag } in records {
            self.push_record(key, self.text.lines.len());
            let mut lines = parse_ansi(text).lines;
            if lines.is_empty() {
                lines.push(Line::default());
            }
            for mut line in lines {
                line.spans.splice(0..0, tag.iter().cloned());
                self.text.lines.push(line);
            }
        }
    }

    fn lay_out(&mut self, wrap: bool, width: u16) {
        self.line_index.rebuild(&self.text, wrap, width);
        self.layout = Some(RowLayout { wrap, width });
        self.generation = self.generation.wrapping_add(1);
        self.last_edit = None;
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
    /// line now in its place: the first line of the first record captured at the same time or
    /// later.
    pub(crate) fn find_record_line(&self, key: RecordLine) -> Result<usize, usize> {
        let found = self.ordinals.get(&key.record.id).and_then(|ordinal| {
            let index = self
                .record_starts
                .binary_search_by_key(ordinal, |start| start.ordinal)
                .ok()?;
            self.record_starts.get(index)
        });
        if let Some(start) = found {
            return Ok(start.first_line + key.line);
        }
        // Records are ordered by capture time; a single service's pane, which leaves that at
        // zero, by sequence.
        let place = |key: RecordKey| (key.timestamp_unix_ms, key.id.seq);
        let record = self
            .record_starts
            .partition_point(|start| place(start.key) < place(key.record));
        Err(self
            .record_starts
            .get(record)
            .map_or(self.text.lines.len(), |start| start.first_line))
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

/// Places lines, taken in order, in a text that lines were cut out of.
#[derive(Debug)]
pub(crate) struct RemovedLines<'a> {
    /// The lines cut out, in order and apart, that come at or after the last line placed.
    ranges: std::iter::Peekable<std::slice::Iter<'a, std::ops::Range<usize>>>,
    /// How many lines were cut out before the last line placed.
    before: usize,
}

impl<'a> RemovedLines<'a> {
    pub(crate) fn new(removed: &'a [std::ops::Range<usize>]) -> Self {
        Self {
            ranges: removed.iter().peekable(),
            before: 0,
        }
    }

    /// Where `line`, numbered as before the cut, is now, or `None` when it was cut out.
    ///
    /// Lines must be placed in order.
    pub(crate) fn place(&mut self, line: usize) -> Option<usize> {
        while let Some(range) = self.ranges.next_if(|range| range.end <= line) {
            self.before += range.end - range.start;
        }
        if self.ranges.peek().is_some_and(|range| range.start <= line) {
            return None;
        }
        Some(line - self.before)
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
