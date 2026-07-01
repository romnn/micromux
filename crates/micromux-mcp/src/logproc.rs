//! Shaping raw session log records for the agent-facing tools.
//!
//! The session returns log *records* exactly as captured. That is the right cursor unit for agents:
//! one record has one `seq`, and `follow_logs(after_seq = seq)` resumes after it. This module keeps
//! that unit intact while stripping surviving SGR escapes by default, trimming terminal padding, and
//! optionally filtering by `grep` or structured-JSON `level`. It runs off the control path: the
//! session stays raw; this shapes what the model reads.

use std::collections::{BTreeMap, VecDeque};

use micromux::{LogLine, StructuredLogLevel, structured_log_level_in_object};
use regex::Regex;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Map, Value};

/// Object keys, matched case-insensitively, under which structured loggers carry timestamps.
const TIMESTAMP_KEYS: &[&str] = &["@timestamp", "timestamp", "time", "ts", "datetime", "date"];

/// Severity ranks for structured-log filtering.
pub type Level = StructuredLogLevel;

/// Detect the level of a line iff it is a JSON object carrying a recognized level field. Returns
/// `None` for any non-JSON (plain text) line — we never guess a level from unstructured output.
#[cfg(test)]
fn detect_level(line: &str) -> Option<Level> {
    parse_json_object(line).and_then(|object| structured_log_level_in_object(&object))
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
        return chrono::DateTime::parse_from_rfc3339(text)
            .ok()
            .and_then(|datetime| u64::try_from(datetime.timestamp_millis()).ok());
    }
    value.as_u64().and_then(numeric_timestamp_to_unix_ms)
}

fn numeric_timestamp_to_unix_ms(value: u64) -> Option<u64> {
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
/// `line` and, when the record was structured JSON, its detected `level`.
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
}

/// How to shape fetched records before returning them to the agent.
#[derive(Default)]
pub struct Shape<'a> {
    /// Keep ANSI color escapes instead of stripping them.
    pub raw: bool,
    /// Keep only entries matching this regex.
    pub grep: Option<&'a Regex>,
    /// Keep only structured-JSON entries at or above this level; drops entries without a JSON
    /// level.
    pub min_level: Option<Level>,
    /// After filtering, keep only the last `limit` entries.
    pub limit: Option<usize>,
}

/// Apply [`Shape`] to fetched records: clean one session record into one agent-facing entry, filter
/// by `grep`/`min_level`, then tail to `limit`. Filtering and level detection always run on the
/// ANSI-stripped text (so they are robust to color escapes and to `raw`); `raw` only controls
/// whether the returned `line` keeps the escapes.
#[must_use]
pub fn shape(records: &[LogLine], options: &Shape) -> Vec<ProcessedEntry> {
    let mut out = Vec::new();
    for record in records {
        let stripped = normalize_record_text(&strip_ansi(&record.line), true);
        if let Some(regex) = options.grep
            && !regex.is_match(&stripped)
        {
            continue;
        }
        let json = parse_json_object(&stripped);
        let level = json.as_ref().and_then(structured_log_level_in_object);
        let source_timestamp_unix_ms = json.as_ref().and_then(source_timestamp_in_object);
        if let Some(min) = options.min_level {
            match level {
                Some(level) if level >= min => {}
                _ => continue,
            }
        }
        out.push(ProcessedEntry {
            service: None,
            seq: record.seq,
            run_generation: record.run_generation,
            timestamp_unix_ms: record.timestamp_unix_ms,
            source_timestamp_unix_ms,
            line: if options.raw {
                normalize_record_text(&record.line, false)
            } else {
                stripped
            },
            level: level.map(Level::canonical),
        });
    }
    if let Some(limit) = options.limit
        && out.len() > limit
    {
        out.drain(0..out.len() - limit);
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
            merged.push(entry);
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
    fn detects_numeric_pino_levels() {
        assert_eq!(
            detect_level(r#"{"level":50,"msg":"x"}"#),
            Some(Level::Error)
        );
        assert_eq!(detect_level(r#"{"level":30}"#), Some(Level::Info));
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
        ];

        let out = shape(&records, &Shape::default());

        assert_eq!(out[0].source_timestamp_unix_ms, Some(1_782_909_296_789));
        assert_eq!(out[1].source_timestamp_unix_ms, Some(1_782_911_696_000));
        assert_eq!(out[2].source_timestamp_unix_ms, Some(1_782_911_696_789));
        assert_eq!(out[3].source_timestamp_unix_ms, Some(1_782_911_696_789));
        assert_eq!(out[4].source_timestamp_unix_ms, Some(1_782_911_696_789));
        assert_eq!(out[5].source_timestamp_unix_ms, None);
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
}
