//! Shaping raw session log records for the agent-facing tools.
//!
//! The session returns log *records* exactly as captured. That is the right cursor unit for agents:
//! one record has one `seq`, and `follow_logs(after_seq = seq)` resumes after it. This module keeps
//! that cursor contract intact while stripping surviving SGR escapes by default, trimming terminal
//! padding, splitting embedded structured-JSON objects out of legacy/snapshot blobs, and optionally
//! filtering by text, time, trace id, or structured-JSON level. It runs off the control path: the
//! session stays raw; this shapes what the model reads.

use std::collections::{BTreeMap, VecDeque};

use micromux::{
    LogLine, StructuredLogLevel, is_structured_log_level_key, structured_log_level_in_object,
};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Object keys, matched case-insensitively, under which structured loggers carry timestamps.
const TIMESTAMP_KEYS: &[&str] = &["@timestamp", "timestamp", "time", "ts", "datetime", "date"];
const MESSAGE_KEYS: &[&str] = &["message", "msg"];
const FIELDS_KEY: &str = "fields";

/// Severity ranks for structured-log filtering.
pub type Level = StructuredLogLevel;

/// Detect the level of a line iff it is a JSON object carrying a recognized level field. Returns
/// `None` for any non-JSON (plain text) line — we never guess a level from unstructured output.
#[cfg(test)]
fn detect_level(line: &str) -> Option<Level> {
    parse_json_object(line).and_then(|object| level_in_object(&object))
}

fn parse_json_object(line: &str) -> Option<Map<String, Value>> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('{') {
        return None;
    }
    match serde_json::from_str::<Value>(trimmed).ok()? {
        Value::Object(object) => Some(object),
        _ => None,
    }
}

struct RecordSegment {
    text: String,
    json: Option<Map<String, Value>>,
}

struct CandidateEntry {
    entry: ProcessedEntry,
    grep_text: String,
}

fn record_segments(text: &str) -> Vec<RecordSegment> {
    if let Some(json) = parse_json_object(text) {
        return vec![RecordSegment {
            text: text.to_string(),
            json: Some(json),
        }];
    }

    let spans = embedded_json_object_spans(text);
    if spans.is_empty() {
        return vec![RecordSegment {
            text: text.to_string(),
            json: None,
        }];
    }

    let mut segments = Vec::new();
    let mut cursor = 0;
    for (start, end, json) in spans {
        push_plain_segment(&mut segments, text.get(cursor..start).unwrap_or_default());
        segments.push(RecordSegment {
            text: text.get(start..end).unwrap_or_default().trim().to_string(),
            json: Some(json),
        });
        cursor = end;
    }
    push_plain_segment(&mut segments, text.get(cursor..).unwrap_or_default());

    // `spans` is non-empty here and each span pushes one JSON segment, so `segments` always has at
    // least one element.
    segments
}

fn embedded_json_object_spans(text: &str) -> Vec<(usize, usize, Map<String, Value>)> {
    let mut spans = Vec::new();
    let mut cursor = 0;
    while cursor < text.len() {
        let Some(relative_start) = text.get(cursor..).and_then(|tail| tail.find('{')) else {
            break;
        };
        let start = cursor + relative_start;
        let Some(slice) = text.get(start..) else {
            break;
        };
        let mut values = serde_json::Deserializer::from_str(slice).into_iter::<Value>();
        match values.next() {
            Some(Ok(Value::Object(object))) => {
                let consumed = values.byte_offset();
                if consumed == 0 {
                    cursor = start.saturating_add(1);
                    continue;
                }
                let end = start.saturating_add(consumed);
                spans.push((start, end, object));
                cursor = end;
            }
            Some(Ok(_) | Err(_)) | None => {
                cursor = start.saturating_add(1);
            }
        }
    }
    spans
}

fn push_plain_segment(segments: &mut Vec<RecordSegment>, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    segments.push(RecordSegment {
        text: text.to_string(),
        json: None,
    });
}

fn source_timestamp_in_object(object: &Map<String, Value>) -> Option<u64> {
    object
        .iter()
        .filter(|(key, _)| {
            TIMESTAMP_KEYS
                .iter()
                .any(|candidate| key.eq_ignore_ascii_case(candidate))
        })
        .find_map(|(_, value)| source_timestamp_of_value(value))
}

fn source_timestamp_of_value(value: &Value) -> Option<u64> {
    if let Some(text) = value.as_str() {
        let text = text.trim();
        if let Ok(number) = text.parse::<u64>() {
            return numeric_timestamp_to_unix_ms(number);
        }
        if let Some(timestamp) = decimal_timestamp_to_unix_ms(text) {
            return Some(timestamp);
        }
        return chrono::DateTime::parse_from_rfc3339(text)
            .ok()
            .and_then(|datetime| u64::try_from(datetime.timestamp_millis()).ok());
    }
    match value {
        Value::Number(number) => number
            .as_u64()
            .and_then(numeric_timestamp_to_unix_ms)
            .or_else(|| decimal_timestamp_to_unix_ms(&number.to_string())),
        Value::Null | Value::Bool(_) | Value::String(_) | Value::Array(_) | Value::Object(_) => {
            None
        }
    }
}

pub(crate) fn numeric_timestamp_to_unix_ms(value: u64) -> Option<u64> {
    if value >= 1_000_000_000_000_000_000 {
        Some(value / 1_000_000)
    } else if value >= 1_000_000_000_000_000 {
        Some(value / 1_000)
    } else if value >= 1_000_000_000_000 {
        Some(value)
    } else if value >= 1_000_000_000 {
        value.checked_mul(1000)
    } else {
        None
    }
}

fn decimal_timestamp_to_unix_ms(raw: &str) -> Option<u64> {
    let (whole, fraction) = raw.split_once('.')?;
    if whole.is_empty() || !whole.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    if !fraction.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let whole = whole.parse::<u64>().ok()?;
    if whole >= 1_000_000_000_000 {
        return numeric_timestamp_to_unix_ms(whole);
    }
    if whole < 1_000_000_000 {
        return None;
    }

    let mut millis = whole.checked_mul(1_000)?;
    let mut fraction_millis = 0_u64;
    let mut scale = 100_u64;
    for digit in fraction.chars().take(3) {
        fraction_millis += u64::from(digit.to_digit(10)?) * scale;
        scale /= 10;
    }
    millis = millis.checked_add(fraction_millis)?;
    Some(millis)
}

fn key_matches(key: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| key.eq_ignore_ascii_case(candidate))
}

fn is_timestamp_key(key: &str) -> bool {
    key_matches(key, TIMESTAMP_KEYS)
}

fn find_key<'a>(
    object: &'a Map<String, Value>,
    candidates: &[&str],
) -> Option<(&'a str, &'a Value)> {
    object
        .iter()
        .find(|(key, _)| key_matches(key, candidates))
        .map(|(key, value)| (key.as_str(), value))
}

fn find_fields_object(object: &Map<String, Value>) -> Option<&Map<String, Value>> {
    object.iter().find_map(|(key, value)| {
        if key.eq_ignore_ascii_case(FIELDS_KEY)
            && let Value::Object(fields) = value
        {
            Some(fields)
        } else {
            None
        }
    })
}

fn level_in_object(object: &Map<String, Value>) -> Option<Level> {
    structured_log_level_in_object(object)
        .or_else(|| find_fields_object(object).and_then(structured_log_level_in_object))
}

fn source_timestamp(object: &Map<String, Value>) -> Option<u64> {
    source_timestamp_in_object(object)
        .or_else(|| find_fields_object(object).and_then(source_timestamp_in_object))
}

fn message_in_object(object: &Map<String, Value>) -> Option<String> {
    find_key(object, MESSAGE_KEYS)
        .or_else(|| find_fields_object(object).and_then(|fields| find_key(fields, MESSAGE_KEYS)))
        .map(|(_, value)| render_value(value))
}

fn structured_fields(object: &Map<String, Value>) -> BTreeMap<String, Value> {
    let mut fields = BTreeMap::new();
    append_fields(&mut fields, object, true);
    if let Some(nested) = find_fields_object(object) {
        append_fields(&mut fields, nested, false);
    }
    fields
}

fn append_fields(
    fields: &mut BTreeMap<String, Value>,
    object: &Map<String, Value>,
    top_level: bool,
) {
    for (key, value) in object {
        if is_structured_log_level_key(key)
            || key_matches(key, MESSAGE_KEYS)
            || is_timestamp_key(key)
        {
            continue;
        }
        if top_level && key.eq_ignore_ascii_case(FIELDS_KEY) && matches!(value, Value::Object(_)) {
            continue;
        }
        fields.entry(key.clone()).or_insert_with(|| value.clone());
    }
}

fn render_value(value: &Value) -> String {
    match value {
        Value::String(text) => sanitize_text(text),
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

fn render_compact_field_value(value: &Value) -> String {
    match value {
        Value::Array(values) => format!("[array:{}]", values.len()),
        Value::Object(fields) => format!("{{object:{}}}", fields.len()),
        Value::String(_) | Value::Null | Value::Bool(_) | Value::Number(_) => render_value(value),
    }
}

fn sanitize_text(text: &str) -> String {
    if !text.chars().any(char::is_control) {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", u32::from(ch));
            }
            ch => out.push(ch),
        }
    }
    out
}

fn timestamp_label(timestamp_unix_ms: u64) -> String {
    let Ok(timestamp) = i64::try_from(timestamp_unix_ms) else {
        return timestamp_unix_ms.to_string();
    };
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(timestamp).map_or_else(
        || timestamp_unix_ms.to_string(),
        |timestamp| timestamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    )
}

fn normalize_record_text(text: &str, flatten_rows: bool) -> String {
    fn drop_cr(part: &str) -> &str {
        part.strip_suffix('\r').unwrap_or(part)
    }

    if !text.contains('\n') {
        return drop_cr(text).trim_end().to_string();
    }

    let parts: Vec<&str> = text.split('\n').map(drop_cr).collect();
    let end = parts
        .iter()
        .rposition(|part| !strip_ansi(part).trim().is_empty())
        .map_or(0, |idx| idx + 1);
    let rows = parts
        .into_iter()
        .take(end)
        .map(str::trim_end)
        .collect::<Vec<_>>();
    if flatten_rows {
        join_visual_rows(&rows)
    } else {
        rows.join("\n")
    }
}

fn join_visual_rows(rows: &[&str]) -> String {
    let mut out = String::new();
    for row in rows
        .iter()
        .map(|row| row.trim())
        .filter(|row| !row.is_empty())
    {
        if out.is_empty() {
            out.push_str(row);
            continue;
        }
        if row
            .chars()
            .next()
            .is_some_and(|ch| matches!(ch, '=' | ',' | ':' | ';' | '.' | ')' | ']' | '}' | '%'))
        {
            out.push_str(row);
        } else {
            out.push(' ');
            out.push_str(row);
        }
    }
    out
}

fn strip_ansi(line: &str) -> String {
    strip_ansi_escapes::strip_str(line)
}

/// A processed, agent-facing log entry: the original record cursor/run/timestamp plus a cleaned
/// `line` and, when the record was structured JSON, its detected `level`, message, and fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ProcessedEntry {
    /// Service id that produced this entry, included for cross-service log queries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Monotonic record cursor. Pass this as `after_seq` to resume after this entry.
    pub seq: u64,
    pub run_generation: u64,
    /// Wall-clock time when micromux ingested this record, in Unix milliseconds.
    pub timestamp_unix_ms: u64,
    /// Wall-clock time parsed from a structured JSON log record, in Unix milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_timestamp_unix_ms: Option<u64>,
    pub line: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, Value>,
}

/// Agent-facing log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    /// Keep `line` as the cleaned logical service record.
    #[default]
    Full,
    /// Return a token-efficient line derived from structured JSON: timestamp, level, message, and
    /// `key=value` fields. Plain text records still return their cleaned line.
    Compact,
}

/// How to shape fetched records before returning them to the agent.
#[derive(Default)]
pub struct Shape<'a> {
    /// Keep ANSI color escapes instead of stripping them.
    pub raw: bool,
    /// Keep only entries matching this regex.
    pub grep: Option<&'a Regex>,
    /// Include this many neighboring entries before and after each grep match.
    pub context: usize,
    /// Keep only structured-JSON entries at or above this level; drops entries without a JSON
    /// level.
    pub min_level: Option<Level>,
    /// Keep only entries at or after this source timestamp (for structured JSON) or micromux
    /// ingestion timestamp (for plain text).
    pub since_unix_ms: Option<u64>,
    /// Keep only entries containing this trace/correlation id.
    pub trace_id: Option<&'a str>,
    /// After filtering, keep only the last `limit` entries.
    pub limit: Option<usize>,
    /// Output format.
    pub format: LogFormat,
}

/// Apply [`Shape`] to fetched records: clean session records into agent-facing entries, split
/// embedded structured-JSON objects out of legacy/snapshot blobs, filter by text/time/level/trace
/// id, then tail to `limit`. Filtering and level detection always run on the ANSI-stripped text (so
/// they are robust to color escapes and to `raw`); `raw` only controls whether an unsplit
/// non-compact plain-text record keeps the escapes.
#[must_use]
pub fn shape(records: &[LogLine], options: &Shape) -> Vec<ProcessedEntry> {
    let mut candidates = Vec::new();
    let apply_grep_per_entry = options.grep.is_none() || options.context == 0;
    for record in records {
        let stripped = normalize_record_text(&strip_ansi(&record.line), true);
        let segments = record_segments(&stripped);
        let raw_line = (options.raw && segments.len() == 1)
            .then(|| normalize_record_text(&record.line, false));
        for segment in segments {
            push_shaped_segment(
                &mut candidates,
                record,
                segment,
                raw_line.as_deref(),
                options,
                apply_grep_per_entry,
            );
        }
    }
    if !apply_grep_per_entry && let Some(regex) = options.grep {
        apply_grep_context(&mut candidates, regex, options.context);
    }
    let mut out = candidates
        .into_iter()
        .map(|candidate| candidate.entry)
        .collect::<Vec<_>>();
    apply_tail_limit(&mut out, options.limit);
    out
}

fn push_shaped_segment(
    out: &mut Vec<CandidateEntry>,
    record: &LogLine,
    segment: RecordSegment,
    raw_line: Option<&str>,
    options: &Shape,
    apply_grep: bool,
) {
    if apply_grep
        && let Some(regex) = options.grep
        && !regex.is_match(&segment.text)
    {
        return;
    }
    let level = segment.json.as_ref().and_then(level_in_object);
    let source_timestamp_unix_ms = segment.json.as_ref().and_then(source_timestamp);
    let effective_timestamp = source_timestamp_unix_ms.unwrap_or(record.timestamp_unix_ms);
    if let Some(since) = options.since_unix_ms
        && effective_timestamp < since
    {
        return;
    }
    if let Some(trace_id) = options.trace_id
        && !segment.text.contains(trace_id)
    {
        return;
    }
    if let Some(min) = options.min_level {
        match level {
            Some(level) if level >= min => {}
            _ => return,
        }
    }
    // grep_text is only consulted by apply_grep_context on the grep+context path; skip the clone
    // (up to MAX_LOG_TAIL entries per call) whenever grep is already matched per-entry above.
    let grep_text = if apply_grep {
        String::new()
    } else {
        segment.text.clone()
    };
    let message = segment.json.as_ref().and_then(message_in_object);
    let fields = segment
        .json
        .as_ref()
        .map_or_else(BTreeMap::new, structured_fields);
    let line = match (options.raw, options.format, segment.json.as_ref()) {
        (_, LogFormat::Compact, Some(_)) => compact_line(
            source_timestamp_unix_ms.unwrap_or(record.timestamp_unix_ms),
            level,
            message.as_deref(),
            &fields,
        ),
        (true, _, _) => raw_line.map_or(segment.text, ToString::to_string),
        _ => segment.text,
    };
    out.push(CandidateEntry {
        entry: ProcessedEntry {
            service: None,
            seq: record.seq,
            run_generation: record.run_generation,
            timestamp_unix_ms: record.timestamp_unix_ms,
            source_timestamp_unix_ms,
            line,
            level: level.map(Level::canonical),
            message,
            fields,
        },
        grep_text,
    });
}

fn apply_grep_context(out: &mut Vec<CandidateEntry>, regex: &Regex, context: usize) {
    let mut keep = vec![false; out.len()];
    for (index, entry) in out.iter().enumerate() {
        if regex.is_match(&entry.grep_text) {
            let start = index.saturating_sub(context);
            let end = index
                .saturating_add(context)
                .min(out.len().saturating_sub(1));
            for slot in keep.iter_mut().take(end + 1).skip(start) {
                *slot = true;
            }
        }
    }
    let mut keep = keep.into_iter();
    out.retain(|_| keep.next().unwrap_or(false));
}

fn apply_tail_limit(out: &mut Vec<ProcessedEntry>, limit: Option<usize>) {
    let Some(limit) = limit else {
        return;
    };
    tail_preserving_record_boundaries(out, limit);
}

pub(crate) fn tail_preserving_record_boundaries(
    entries: &mut Vec<ProcessedEntry>,
    limit: usize,
) -> bool {
    if limit == 0 {
        let truncated = !entries.is_empty();
        entries.clear();
        return truncated;
    }
    if entries.len() <= limit {
        return false;
    }
    let mut start = entries.len() - limit;
    let Some(first_kept) = entries.get(start) else {
        return false;
    };
    let boundary = RecordBoundary::of(first_kept);
    while start > 0
        && entries
            .get(start - 1)
            .is_some_and(|entry| boundary.matches(entry))
    {
        start -= 1;
    }
    let truncated = start > 0;
    entries.drain(0..start);
    truncated
}

pub(crate) fn truncate_preserving_record_boundaries(
    entries: &mut Vec<ProcessedEntry>,
    limit: usize,
) -> bool {
    if limit == 0 {
        let truncated = !entries.is_empty();
        entries.clear();
        return truncated;
    }
    if entries.len() <= limit {
        return false;
    }
    let mut end = limit;
    let Some(last_kept) = entries.get(end - 1) else {
        return false;
    };
    let boundary = RecordBoundary::of(last_kept);
    while end < entries.len()
        && entries
            .get(end)
            .is_some_and(|entry| boundary.matches(entry))
    {
        end += 1;
    }
    let truncated = end < entries.len();
    entries.truncate(end);
    truncated
}

struct RecordBoundary<'a> {
    service: Option<&'a str>,
    run_generation: u64,
    seq: u64,
}

impl<'a> RecordBoundary<'a> {
    fn of(entry: &'a ProcessedEntry) -> Self {
        Self {
            service: entry.service.as_deref(),
            run_generation: entry.run_generation,
            seq: entry.seq,
        }
    }

    fn matches(&self, entry: &ProcessedEntry) -> bool {
        entry.service.as_deref() == self.service
            && entry.run_generation == self.run_generation
            && entry.seq == self.seq
    }
}

fn compact_line(
    timestamp_unix_ms: u64,
    level: Option<Level>,
    message: Option<&str>,
    fields: &BTreeMap<String, Value>,
) -> String {
    let mut out = timestamp_label(timestamp_unix_ms);
    if let Some(level) = level {
        out.push(' ');
        out.push_str(&level.canonical().to_ascii_uppercase());
    }
    if let Some(message) = message
        && !message.is_empty()
    {
        out.push(' ');
        out.push_str(message);
    }
    for (key, value) in fields {
        out.push(' ');
        out.push_str(&sanitize_text(key));
        out.push('=');
        out.push_str(&render_compact_field_value(value));
    }
    out
}

#[must_use]
pub(crate) fn merge_preserving_service_order(entries: Vec<ProcessedEntry>) -> Vec<ProcessedEntry> {
    let mut groups: BTreeMap<Option<String>, VecDeque<ProcessedEntry>> = BTreeMap::new();
    for entry in entries {
        groups
            .entry(entry.service.clone())
            .or_default()
            .push_back(entry);
    }

    let total = groups.values().map(VecDeque::len).sum();
    let mut merged = Vec::with_capacity(total);
    while !groups.is_empty() {
        // Do not globally sort all entries: parsed service timestamps can arrive out of order, but
        // cursors are per-service sequence numbers. Pick the next best service head so each
        // service's own seq order is preserved.
        let selected = groups
            .iter()
            .filter_map(|(service, entries)| entries.front().map(|entry| (service.clone(), entry)))
            .min_by(|(left_service, left), (right_service, right)| {
                entry_sort_timestamp(left)
                    .cmp(&entry_sort_timestamp(right))
                    .then_with(|| left_service.cmp(right_service))
                    .then_with(|| left.seq.cmp(&right.seq))
            })
            .map(|(service, _)| service);
        let Some(service) = selected else {
            break;
        };
        let Some(group) = groups.get_mut(&service) else {
            break;
        };
        if let Some(entry) = group.pop_front() {
            // One captured record (a single seq) can shape into several entries when it carries
            // embedded JSON. Emit them as one contiguous block: interleaving another service
            // between them would defeat the boundary-preserving tail/truncate helpers, which only
            // reattach *adjacent* same-seq entries — a split there drops half a record and advances
            // that service's cursor past the dropped half.
            let (run_generation, seq) = (entry.run_generation, entry.seq);
            merged.push(entry);
            while group
                .front()
                .is_some_and(|next| next.run_generation == run_generation && next.seq == seq)
            {
                let Some(next) = group.pop_front() else {
                    break;
                };
                merged.push(next);
            }
        }
        if group.is_empty() {
            groups.remove(&service);
        }
    }
    merged
}

fn entry_sort_timestamp(entry: &ProcessedEntry) -> u64 {
    entry
        .source_timestamp_unix_ms
        .unwrap_or(entry.timestamp_unix_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use similar_asserts::assert_eq;

    fn record(seq: u64, line: &str) -> LogLine {
        LogLine {
            seq,
            run_generation: 1,
            timestamp_unix_ms: 1_700_000_000_000 + seq,
            line: line.to_string(),
        }
    }

    fn lines(processed: &[ProcessedEntry]) -> Vec<&str> {
        processed.iter().map(|p| p.line.as_str()).collect()
    }

    #[test]
    fn strips_ansi_color_codes_by_default() {
        let records = vec![record(1, "\x1b[31mred\x1b[0m text")];
        let out = shape(&records, &Shape::default());
        assert_eq!(lines(&out), vec!["red text"]);
    }

    #[test]
    fn raw_keeps_ansi() {
        let records = vec![record(1, "\x1b[31mred\x1b[0m")];
        let out = shape(
            &records,
            &Shape {
                raw: true,
                ..Shape::default()
            },
        );
        assert_eq!(lines(&out), vec!["\x1b[31mred\x1b[0m"]);
    }

    #[test]
    fn compact_mode_still_honors_raw_for_plain_text() {
        let records = vec![record(1, "\x1b[31mplain\x1b[0m")];
        let out = shape(
            &records,
            &Shape {
                raw: true,
                format: LogFormat::Compact,
                ..Shape::default()
            },
        );
        assert_eq!(lines(&out), vec!["\x1b[31mplain\x1b[0m"]);
    }

    #[test]
    fn keeps_a_snapshot_record_together_and_trims_blank_bottom() {
        let records = vec![record(7, "row a   \nrow b\t\nrow c     \n\n\n")];
        let out = shape(&records, &Shape::default());
        assert_eq!(lines(&out), vec!["row a row b row c"]);
        assert_eq!(out[0].seq, 7);
    }

    #[test]
    fn raw_preserves_snapshot_rows() {
        let records = vec![record(7, "\x1b[31mrow a\x1b[0m   \nrow b     \n\n")];
        let out = shape(
            &records,
            &Shape {
                raw: true,
                ..Shape::default()
            },
        );
        assert_eq!(lines(&out), vec!["\x1b[31mrow a\x1b[0m\nrow b"]);
    }

    #[test]
    fn keeps_an_intentionally_blank_single_line_record() {
        let records = vec![record(1, ""), record(2, "after")];
        let out = shape(&records, &Shape::default());
        assert_eq!(lines(&out), vec!["", "after"]);
    }

    #[test]
    fn tail_limit_counts_records_not_visual_lines() {
        let records = vec![record(1, "a\nb\nc\nd"), record(2, "next")];
        let out = shape(
            &records,
            &Shape {
                limit: Some(1),
                ..Shape::default()
            },
        );
        assert_eq!(lines(&out), vec!["next"]);
    }

    #[test]
    fn grep_filters_to_matching_lines() {
        let records = vec![
            record(1, "starting up"),
            record(2, "ERROR boom"),
            record(3, "ok"),
        ];
        let regex = Regex::new("ERROR").unwrap();
        let out = shape(
            &records,
            &Shape {
                grep: Some(&regex),
                ..Shape::default()
            },
        );
        assert_eq!(lines(&out), vec!["ERROR boom"]);
    }

    #[test]
    fn grep_context_includes_neighboring_entries() {
        let records = vec![
            record(1, "before"),
            record(2, "ERROR boom"),
            record(3, "after"),
            record(4, "later"),
        ];
        let regex = Regex::new("ERROR").unwrap();
        let out = shape(
            &records,
            &Shape {
                grep: Some(&regex),
                context: 1,
                ..Shape::default()
            },
        );

        assert_eq!(lines(&out), vec!["before", "ERROR boom", "after"]);
    }

    #[test]
    fn grep_context_matches_cleaned_text_not_formatted_line() {
        let records = vec![
            record(1, "before"),
            record(2, r#"{"level":"error","msg":"boom"}"#),
            record(3, "after"),
        ];
        let regex = Regex::new(r#""level":"error""#).unwrap();
        let out = shape(
            &records,
            &Shape {
                grep: Some(&regex),
                context: 1,
                format: LogFormat::Compact,
                ..Shape::default()
            },
        );

        assert_eq!(out.len(), 3);
        assert_eq!(out[0].line, "before");
        assert!(out[1].line.contains("ERROR boom"));
        assert_eq!(out[2].line, "after");
    }

    #[test]
    fn grep_matches_across_wrapped_record_boundaries() {
        let records = vec![record(
            45,
            "chat completion request provider=ollama num_messages   \n=2 body_bytes=1465512    ",
        )];

        let broad = Regex::new("chat completion request.*body_bytes").unwrap();
        let exact = Regex::new("num_messages=2").unwrap();

        let out = shape(
            &records,
            &Shape {
                grep: Some(&broad),
                ..Shape::default()
            },
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].seq, 45);
        assert_eq!(
            out[0].line,
            "chat completion request provider=ollama num_messages=2 body_bytes=1465512"
        );

        let out = shape(
            &records,
            &Shape {
                grep: Some(&exact),
                ..Shape::default()
            },
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn min_level_keeps_structured_lines_at_or_above_and_drops_plain_lines() {
        let records = vec![
            record(1, r#"{"level":"info","msg":"hi"}"#),
            record(2, r#"{"level":"error","msg":"bad"}"#),
            record(3, "a plain line with no level"),
            record(4, r#"{"severity":"WARN","msg":"careful"}"#),
        ];
        let out = shape(
            &records,
            &Shape {
                min_level: Some(Level::Warn),
                ..Shape::default()
            },
        );
        // info dropped (too low), plain dropped (no JSON level), error+warn kept.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].line, r#"{"level":"error","msg":"bad"}"#);
        assert_eq!(out[0].level, Some("error"));
        assert_eq!(out[1].level, Some("warn"));
    }

    #[test]
    fn splits_embedded_json_objects_before_level_filtering() {
        let records = vec![record(
            76,
            concat!(
                r#"{"timestamp":"2026-07-01T12:00:00Z","level":"INFO","fields":{"message":"setup ok"}}"#,
                " ",
                r#"{"timestamp":"2026-07-01T12:00:01Z","level":"ERROR","fields":{"message":"startup backfill failed","code":"missing_relation"}}"#,
            ),
        )];

        let out = shape(
            &records,
            &Shape {
                min_level: Some(Level::Error),
                format: LogFormat::Compact,
                ..Shape::default()
            },
        );

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].seq, 76);
        assert_eq!(out[0].level, Some("error"));
        assert_eq!(out[0].message.as_deref(), Some("startup backfill failed"));
        assert_eq!(
            out[0].line,
            "2026-07-01T12:00:01.000Z ERROR startup backfill failed code=missing_relation"
        );
    }

    #[test]
    fn split_json_blob_keeps_plain_error_suffix_searchable() {
        let records = vec![record(
            76,
            concat!(
                r#"{"level":"INFO","fields":{"message":"setup ok"}}"#,
                " Error: relation \"app.credit_submission_file_classifications\" does not exist",
            ),
        )];
        let regex = Regex::new("credit_submission_file_classifications").unwrap();

        let out = shape(
            &records,
            &Shape {
                grep: Some(&regex),
                ..Shape::default()
            },
        );

        assert_eq!(
            lines(&out),
            vec![r#"Error: relation "app.credit_submission_file_classifications" does not exist"#]
        );
        assert_eq!(out[0].seq, 76);
    }

    #[test]
    fn tail_limit_does_not_split_entries_from_one_record() {
        let records = vec![record(
            9,
            concat!(
                r#"{"level":"INFO","msg":"one"}"#,
                " ",
                r#"{"level":"INFO","msg":"two"}"#,
                " ",
                r#"{"level":"INFO","msg":"three"}"#,
            ),
        )];

        let out = shape(
            &records,
            &Shape {
                limit: Some(1),
                ..Shape::default()
            },
        );

        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|entry| entry.seq == 9));
    }

    #[test]
    fn tail_boundary_helper_reports_false_when_nothing_is_dropped() {
        let records = vec![record(
            9,
            concat!(
                r#"{"level":"INFO","msg":"one"}"#,
                " ",
                r#"{"level":"INFO","msg":"two"}"#,
            ),
        )];
        let mut out = shape(&records, &Shape::default());

        let truncated = tail_preserving_record_boundaries(&mut out, 1);

        assert!(!truncated);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|entry| entry.seq == 9));
    }

    #[test]
    fn zero_tail_limit_returns_no_entries() {
        let records = vec![record(1, "one")];
        let out = shape(
            &records,
            &Shape {
                limit: Some(0),
                ..Shape::default()
            },
        );

        assert_eq!(out.len(), 0);
    }

    fn processed(service: &str, seq: u64, source_ts: u64) -> ProcessedEntry {
        ProcessedEntry {
            service: Some(service.to_string()),
            seq,
            run_generation: 1,
            timestamp_unix_ms: source_ts,
            source_timestamp_unix_ms: Some(source_ts),
            line: format!("{service}:{seq}@{source_ts}"),
            level: None,
            message: None,
            fields: BTreeMap::new(),
        }
    }

    #[test]
    fn merge_keeps_split_record_segments_contiguous() {
        // api's single record (seq 5) shaped into two embedded-JSON entries whose source
        // timestamps straddle worker's entry. The merge must emit the two seq-5 entries adjacently
        // rather than interleaving worker between them; otherwise boundary-preserving truncation
        // would later drop half of record 5 and advance api's cursor past the dropped half.
        let entries = vec![
            processed("api", 5, 100),
            processed("api", 5, 102),
            processed("worker", 3, 101),
        ];

        let merged = merge_preserving_service_order(entries);

        let order: Vec<(&str, u64, u64)> = merged
            .iter()
            .map(|entry| {
                (
                    entry.service.as_deref().unwrap_or_default(),
                    entry.seq,
                    entry.source_timestamp_unix_ms.unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            order,
            vec![("api", 5, 100), ("api", 5, 102), ("worker", 3, 101)]
        );
    }

    #[test]
    fn detects_numeric_pino_levels() {
        assert_eq!(
            detect_level(r#"{"level":50,"msg":"x"}"#),
            Some(Level::Error)
        );
        assert_eq!(detect_level(r#"{"level":30}"#), Some(Level::Info));
        assert_eq!(
            detect_level(r#"{"fields":{"level":"warn"}}"#),
            Some(Level::Warn)
        );
    }

    #[test]
    fn surfaces_detected_level_on_entries() {
        let records = vec![record(1, r#"{"level":"debug","msg":"trace it"}"#)];
        let out = shape(&records, &Shape::default());
        assert_eq!(out[0].level, Some("debug"));
        assert_eq!(out[0].timestamp_unix_ms, 1_700_000_000_001);
        assert_eq!(out[0].source_timestamp_unix_ms, None);
    }

    #[test]
    fn parses_structured_json_timestamps() {
        let records = vec![
            record(
                1,
                r#"{"timestamp":"2026-07-01T12:34:56.789Z","msg":"rfc3339"}"#,
            ),
            record(2, r#"{"time":1782911696,"msg":"seconds"}"#),
            record(3, r#"{"ts":1782911696789,"msg":"millis"}"#),
            record(4, r#"{"timestamp":1782911696789000,"msg":"micros"}"#),
            record(5, r#"{"timestamp":1782911696789000000,"msg":"nanos"}"#),
            record(6, r#"{"timestamp":42,"msg":"counter"}"#),
            record(7, r#"{"timestamp":1782911696.789,"msg":"float seconds"}"#),
            record(
                8,
                r#"{"timestamp":"1782911696.789","msg":"string float seconds"}"#,
            ),
            record(
                9,
                r#"{"fields":{"time":1782911696.789},"msg":"nested float seconds"}"#,
            ),
        ];

        let out = shape(&records, &Shape::default());

        assert_eq!(out[0].source_timestamp_unix_ms, Some(1_782_909_296_789));
        assert_eq!(out[1].source_timestamp_unix_ms, Some(1_782_911_696_000));
        assert_eq!(out[2].source_timestamp_unix_ms, Some(1_782_911_696_789));
        assert_eq!(out[3].source_timestamp_unix_ms, Some(1_782_911_696_789));
        assert_eq!(out[4].source_timestamp_unix_ms, Some(1_782_911_696_789));
        assert_eq!(out[5].source_timestamp_unix_ms, None);
        assert_eq!(out[6].source_timestamp_unix_ms, Some(1_782_911_696_789));
        assert_eq!(out[7].source_timestamp_unix_ms, Some(1_782_911_696_789));
        assert_eq!(out[8].source_timestamp_unix_ms, Some(1_782_911_696_789));
    }

    #[test]
    fn plain_lines_have_no_level() {
        let records = vec![record(1, "just text"), record(2, "{not json")];
        let out = shape(&records, &Shape::default());
        assert!(out.iter().all(|line| line.level.is_none()));
    }

    #[test]
    fn grep_matches_on_stripped_text_even_when_raw() {
        // raw=true must still filter against the stripped line, or an anchored pattern never matches
        // the colored output the agent is hunting.
        let records = vec![record(1, "\x1b[31mERROR\x1b[0m boom")];
        let regex = Regex::new("^ERROR").unwrap();
        let out = shape(
            &records,
            &Shape {
                raw: true,
                grep: Some(&regex),
                ..Shape::default()
            },
        );
        assert_eq!(lines(&out), vec!["\x1b[31mERROR\x1b[0m boom"]);
    }

    #[test]
    fn min_level_detects_on_stripped_json_even_when_raw() {
        let records = vec![record(
            1,
            "\x1b[2m{\"level\":\"error\",\"msg\":\"x\"}\x1b[0m",
        )];
        let out = shape(
            &records,
            &Shape {
                raw: true,
                min_level: Some(Level::Warn),
                ..Shape::default()
            },
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].level, Some("error"));
        // Output still carries the original ANSI because raw was requested.
        assert!(out[0].line.contains('\x1b'));
    }

    #[test]
    fn strips_trailing_cr_from_crlf_lines() {
        let records = vec![record(1, "first\r\nsecond\r"), record(2, "lone\r")];
        let out = shape(&records, &Shape::default());
        assert_eq!(lines(&out), vec!["first second", "lone"]);
    }

    #[test]
    fn classifies_float_encoded_numeric_levels() {
        assert_eq!(detect_level(r#"{"level":30.0}"#), Some(Level::Info));
        assert_eq!(
            detect_level(r#"{"level":5e1,"msg":"x"}"#),
            Some(Level::Error)
        );
    }

    #[test]
    fn compact_json_logs_parse_message_fields_and_level() {
        let records = vec![record(
            1,
            r#"{"timestamp":"2026-07-01T12:34:56.789Z","level":"warn","msg":"slow request","fields":{"trace_id":"abc","elapsed_ms":42},"path":"/api"}"#,
        )];

        let out = shape(
            &records,
            &Shape {
                format: LogFormat::Compact,
                ..Shape::default()
            },
        );

        assert_eq!(out[0].level, Some("warn"));
        assert_eq!(out[0].message.as_deref(), Some("slow request"));
        assert_eq!(
            out[0].fields.get("trace_id").and_then(Value::as_str),
            Some("abc")
        );
        assert_eq!(
            out[0].fields.get("elapsed_ms").and_then(Value::as_u64),
            Some(42)
        );
        assert_eq!(
            out[0].fields.get("path").and_then(Value::as_str),
            Some("/api")
        );
        assert_eq!(
            out[0].line,
            "2026-07-01T12:34:56.789Z WARN slow request elapsed_ms=42 path=/api trace_id=abc"
        );
        assert!(!out[0].line.contains("fields="));
    }

    #[test]
    fn since_filters_on_source_timestamp_when_present() {
        let records = vec![
            record(1, r#"{"timestamp":"2026-07-01T12:00:00Z","msg":"old"}"#),
            record(2, r#"{"timestamp":"2026-07-01T12:00:01Z","msg":"new"}"#),
        ];

        let out = shape(
            &records,
            &Shape {
                since_unix_ms: Some(1_782_907_201_000),
                ..Shape::default()
            },
        );

        assert_eq!(
            lines(&out),
            vec![r#"{"timestamp":"2026-07-01T12:00:01Z","msg":"new"}"#]
        );
    }

    #[test]
    fn trace_id_filters_on_cleaned_log_text() {
        let records = vec![
            record(1, r#"{"trace_id":"abc","msg":"hit"}"#),
            record(2, r#"{"trace_id":"def","msg":"miss"}"#),
        ];

        let out = shape(
            &records,
            &Shape {
                trace_id: Some("abc"),
                ..Shape::default()
            },
        );

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].message.as_deref(), Some("hit"));
    }

    #[test]
    fn compact_line_summarizes_nested_values_but_fields_keep_them_typed() {
        let records = vec![record(
            1,
            r#"{"level":"info","msg":"with spans","spans":[{"name":"root"},{"name":"child"}],"attrs":{"x":1},"bad\u001b[31m":1}"#,
        )];

        let out = shape(
            &records,
            &Shape {
                format: LogFormat::Compact,
                ..Shape::default()
            },
        );

        assert!(out[0].line.contains("spans=[array:2]"));
        assert!(out[0].line.contains("attrs={object:1}"));
        assert!(out[0].line.contains("bad\\u001b[31m=1"));
        assert!(out[0].fields.get("spans").is_some_and(Value::is_array));
        assert!(out[0].fields.get("attrs").is_some_and(Value::is_object));
    }
}
