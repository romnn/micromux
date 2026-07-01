//! Shaping raw session log records for the agent-facing tools.
//!
//! The session returns log *records* exactly as captured, which is wrong for an agent on two counts:
//! a record can carry raw ANSI color escapes (token-heavy) and a single interactive-snapshot record
//! (e.g. a `cargo` progress frame) embeds many visual lines under one `seq`, so `tail` would count
//! frames, not lines. This module strips the surviving SGR escapes, splits a record into visual
//! lines so `tail` is meaningful, and optionally filters by a `grep` regex or by structured-JSON
//! `level`. It runs off the control path: the session stays raw; this shapes what the model reads.

use micromux::LogLine;
use regex::Regex;
use schemars::JsonSchema;
use serde::Serialize;

/// Object keys, matched case-insensitively, under which structured loggers carry the level.
const LEVEL_KEYS: &[&str] = &["level", "lvl", "severity", "levelname", "loglevel"];

/// Textual level synonyms, matched case-insensitively, mapped to a severity rank.
const LEVEL_WORDS: &[(&str, Level)] = &[
    ("trace", Level::Trace),
    ("debug", Level::Debug),
    ("dbg", Level::Debug),
    ("info", Level::Info),
    ("information", Level::Info),
    ("notice", Level::Info),
    ("warn", Level::Warn),
    ("warning", Level::Warn),
    ("error", Level::Error),
    ("err", Level::Error),
    ("fatal", Level::Fatal),
    ("critical", Level::Fatal),
    ("crit", Level::Fatal),
    ("panic", Level::Fatal),
    ("emerg", Level::Fatal),
    ("emergency", Level::Fatal),
    ("alert", Level::Fatal),
];

/// Severity ranks for structured-log filtering, ordered so `>=` means "at least this severe".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl Level {
    fn canonical(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Fatal => "fatal",
        }
    }

    /// Parse a textual level (case-insensitive), accepting the common synonyms loggers emit.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        let word = word.trim();
        LEVEL_WORDS
            .iter()
            .find(|(name, _)| word.eq_ignore_ascii_case(name))
            .map(|(_, level)| *level)
    }

    /// Map a numeric level to a rank, following the pino/bunyan convention (trace=10 … fatal=60)
    /// that dominates JSON loggers emitting numeric levels. Takes `f64` so float-encoded levels
    /// (`30.0`, `3e1`) classify instead of being treated as level-less.
    fn from_number(number: f64) -> Self {
        if number <= 15.0 {
            Self::Trace
        } else if number <= 25.0 {
            Self::Debug
        } else if number <= 35.0 {
            Self::Info
        } else if number <= 45.0 {
            Self::Warn
        } else if number <= 55.0 {
            Self::Error
        } else {
            Self::Fatal
        }
    }
}

/// Detect the level of a line iff it is a JSON object carrying a recognized level field. Returns
/// `None` for any non-JSON (plain text) line — we never guess a level from unstructured output.
fn detect_level(line: &str) -> Option<Level> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('{') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    value
        .as_object()?
        .iter()
        .filter(|(key, _)| {
            LEVEL_KEYS
                .iter()
                .any(|candidate| key.eq_ignore_ascii_case(candidate))
        })
        .find_map(|(_, val)| level_of_value(val))
}

/// Interpret a JSON level value: a textual synonym, or a numeric (pino/bunyan) rank. Reads numbers
/// via `as_f64` so integer *and* float/exponential encodings both classify.
fn level_of_value(value: &serde_json::Value) -> Option<Level> {
    if let Some(text) = value.as_str() {
        Level::parse(text)
    } else {
        value.as_f64().map(Level::from_number)
    }
}

/// Split a record into visual lines, returning each line with ANSI preserved but its trailing CRLF
/// `\r` stripped. A single-line record (including an intentionally blank one) is returned verbatim;
/// a multi-line snapshot frame is split on newlines with its blank screen-bottom trimmed, so `tail`
/// over the result counts content lines rather than empty grid rows.
fn split_visual(text: &str) -> Vec<&str> {
    fn drop_cr(part: &str) -> &str {
        part.strip_suffix('\r').unwrap_or(part)
    }

    if !text.contains('\n') {
        return vec![drop_cr(text)];
    }
    let parts: Vec<&str> = text.split('\n').map(drop_cr).collect();
    // Blankness is judged on the stripped text so an ANSI-only grid row counts as blank.
    let end = parts
        .iter()
        .rposition(|part| !strip_ansi(part).trim().is_empty())
        .map_or(0, |idx| idx + 1);
    parts.into_iter().take(end).collect()
}

fn strip_ansi(line: &str) -> String {
    strip_ansi_escapes::strip_str(line)
}

/// A processed, agent-facing log line: the original cursor/run plus a cleaned `line` and, when the
/// record was structured JSON, its detected `level`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ProcessedLine {
    pub seq: u64,
    pub run_generation: u64,
    pub line: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<&'static str>,
}

/// How to shape fetched records before returning them to the agent.
#[derive(Default)]
pub struct Shape<'a> {
    /// Keep ANSI color escapes instead of stripping them.
    pub raw: bool,
    /// Keep only lines matching this regex.
    pub grep: Option<&'a Regex>,
    /// Keep only structured-JSON lines at or above this level; drops lines without a JSON level.
    pub min_level: Option<Level>,
    /// After splitting and filtering, keep only the last `limit` lines.
    pub limit: Option<usize>,
}

/// Apply [`Shape`] to fetched records: split into visual lines, filter by `grep`/`min_level`, then
/// tail to `limit`. Filtering and level detection always run on the ANSI-stripped text (so they are
/// robust to color escapes and to `raw`); `raw` only controls whether the returned `line` keeps the
/// escapes.
#[must_use]
pub fn shape(records: &[LogLine], options: &Shape) -> Vec<ProcessedLine> {
    let mut out = Vec::new();
    for record in records {
        for raw_line in split_visual(&record.line) {
            let stripped = strip_ansi(raw_line);
            if let Some(regex) = options.grep
                && !regex.is_match(&stripped)
            {
                continue;
            }
            let level = detect_level(&stripped);
            if let Some(min) = options.min_level {
                match level {
                    Some(level) if level >= min => {}
                    _ => continue,
                }
            }
            out.push(ProcessedLine {
                seq: record.seq,
                run_generation: record.run_generation,
                line: if options.raw {
                    raw_line.to_string()
                } else {
                    stripped
                },
                level: level.map(Level::canonical),
            });
        }
    }
    if let Some(limit) = options.limit
        && out.len() > limit
    {
        out.drain(0..out.len() - limit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use similar_asserts::assert_eq;

    fn record(seq: u64, line: &str) -> LogLine {
        LogLine {
            seq,
            run_generation: 1,
            line: line.to_string(),
        }
    }

    fn lines(processed: &[ProcessedLine]) -> Vec<&str> {
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
    fn splits_a_snapshot_record_into_visual_lines_and_trims_blank_bottom() {
        // One record, three content rows then blank grid rows, under a single seq.
        let records = vec![record(7, "row a\nrow b\nrow c\n\n\n")];
        let out = shape(&records, &Shape::default());
        assert_eq!(lines(&out), vec!["row a", "row b", "row c"]);
        // Every split line keeps the originating record's seq.
        assert!(out.iter().all(|line| line.seq == 7));
    }

    #[test]
    fn keeps_an_intentionally_blank_single_line_record() {
        let records = vec![record(1, ""), record(2, "after")];
        let out = shape(&records, &Shape::default());
        assert_eq!(lines(&out), vec!["", "after"]);
    }

    #[test]
    fn tail_limit_counts_visual_lines_not_records() {
        let records = vec![record(1, "a\nb\nc\nd")];
        let out = shape(
            &records,
            &Shape {
                limit: Some(2),
                ..Shape::default()
            },
        );
        assert_eq!(lines(&out), vec!["c", "d"]);
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
        assert_eq!(lines(&out), vec!["first", "second", "lone"]);
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
