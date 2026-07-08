//! Shared helpers for recognizing and displaying structured JSON log records.

use serde_json::{Map, Value};

const STRUCTURED_LOG_LEVEL_KEYS: &[&str] = &["level", "lvl", "severity", "levelname", "loglevel"];
/// Keys commonly used for a structured log message body.
pub const MESSAGE_KEYS: &[&str] = &["message", "msg"];
/// The tracing-subscriber style nested fields object key.
pub const FIELDS_KEY: &str = "fields";

const LEVEL_WORDS: &[(&str, StructuredLogLevel)] = &[
    ("trace", StructuredLogLevel::Trace),
    ("debug", StructuredLogLevel::Debug),
    ("dbg", StructuredLogLevel::Debug),
    ("info", StructuredLogLevel::Info),
    ("information", StructuredLogLevel::Info),
    ("notice", StructuredLogLevel::Info),
    ("warn", StructuredLogLevel::Warn),
    ("warning", StructuredLogLevel::Warn),
    ("error", StructuredLogLevel::Error),
    ("err", StructuredLogLevel::Error),
    ("fatal", StructuredLogLevel::Fatal),
    ("critical", StructuredLogLevel::Fatal),
    ("crit", StructuredLogLevel::Fatal),
    ("panic", StructuredLogLevel::Fatal),
    ("emerg", StructuredLogLevel::Fatal),
    ("emergency", StructuredLogLevel::Fatal),
    ("alert", StructuredLogLevel::Fatal),
];

/// Severity ranks for structured logs, ordered so `>=` means "at least this severe".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StructuredLogLevel {
    /// Trace-level diagnostic output.
    Trace,
    /// Debug-level diagnostic output.
    Debug,
    /// Informational output.
    Info,
    /// Warning output.
    Warn,
    /// Error output.
    Error,
    /// Fatal or critical output.
    Fatal,
}

impl StructuredLogLevel {
    /// Return the canonical lower-case level name.
    #[must_use]
    pub fn canonical(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::Fatal => "fatal",
        }
    }

    /// Parse a textual level, accepting common structured-logger synonyms.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        let word = word.trim();
        LEVEL_WORDS
            .iter()
            .find(|(candidate, _)| word.eq_ignore_ascii_case(candidate))
            .map(|(_, level)| *level)
    }

    /// Map a numeric level to a rank, following the pino/bunyan convention.
    ///
    /// The pino/bunyan convention (`trace=10` through `fatal=60`) dominates JSON loggers emitting
    /// numeric levels. Takes `f64` so float-encoded levels (`30.0`, `3e1`) classify instead of
    /// being treated as level-less.
    #[must_use]
    pub fn from_number(number: f64) -> Self {
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

    /// Interpret a JSON level value: either a textual synonym or a numeric pino/bunyan rank.
    #[must_use]
    pub fn from_value(value: &Value) -> Option<Self> {
        if let Some(text) = value.as_str() {
            Self::parse(text)
        } else {
            value.as_f64().map(Self::from_number)
        }
    }
}

/// Return whether `key` is a recognized structured-log level key.
#[must_use]
pub fn is_structured_log_level_key(key: &str) -> bool {
    STRUCTURED_LOG_LEVEL_KEYS
        .iter()
        .any(|candidate| key.eq_ignore_ascii_case(candidate))
}

/// Return whether `key` matches one of the candidate keys, case-insensitively.
#[must_use]
pub fn key_matches(key: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| key.eq_ignore_ascii_case(candidate))
}

/// Find the first object field whose key matches the candidates, case-insensitively.
#[must_use]
pub fn find_key<'a>(
    object: &'a Map<String, Value>,
    candidates: &[&str],
) -> Option<(&'a str, &'a Value)> {
    object
        .iter()
        .find(|(key, _)| key_matches(key, candidates))
        .map(|(key, value)| (key.as_str(), value))
}

/// Find the tracing-style nested `fields` object, if present.
#[must_use]
pub fn find_fields_object(object: &Map<String, Value>) -> Option<&Map<String, Value>> {
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

/// Detect the level of a JSON log object from its recognized level fields.
#[must_use]
pub fn structured_log_level_in_object(object: &Map<String, Value>) -> Option<StructuredLogLevel> {
    object
        .iter()
        .filter(|(key, _)| is_structured_log_level_key(key))
        .find_map(|(_, value)| StructuredLogLevel::from_value(value))
}

/// Detect a level on the top-level record, falling back to a nested tracing-style `fields` object.
#[must_use]
pub fn structured_log_level_in_record(object: &Map<String, Value>) -> Option<StructuredLogLevel> {
    structured_log_level_in_object(object)
        .or_else(|| find_fields_object(object).and_then(structured_log_level_in_object))
}

/// Escape control characters so a log payload cannot inject terminal control sequences.
#[must_use]
pub fn sanitize_text(text: &str) -> String {
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

/// Render a JSON scalar for display; arrays and objects use compact JSON.
#[must_use]
pub fn render_scalar(value: &Value) -> String {
    match value {
        Value::String(text) => sanitize_text(text),
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        StructuredLogLevel, sanitize_text, structured_log_level_in_object,
        structured_log_level_in_record,
    };
    use similar_asserts::assert_eq;

    fn detect(line: &str) -> Option<StructuredLogLevel> {
        let serde_json::Value::Object(object) =
            serde_json::from_str::<serde_json::Value>(line).ok()?
        else {
            return None;
        };
        structured_log_level_in_object(&object)
    }

    #[test]
    fn detects_textual_level_synonyms() {
        assert_eq!(
            detect(r#"{"severity":"WARN"}"#),
            Some(StructuredLogLevel::Warn)
        );
        assert_eq!(
            detect(r#"{"levelname":"critical"}"#),
            Some(StructuredLogLevel::Fatal)
        );
    }

    #[test]
    fn detects_numeric_pino_levels() {
        assert_eq!(detect(r#"{"level":30}"#), Some(StructuredLogLevel::Info));
        assert_eq!(detect(r#"{"level":5e1}"#), Some(StructuredLogLevel::Error));
    }

    #[test]
    fn falls_back_to_tracing_fields_for_level() {
        let serde_json::Value::Object(object) =
            serde_json::from_str::<serde_json::Value>(r#"{"fields":{"level":"error"}}"#).unwrap()
        else {
            panic!("expected object");
        };

        assert_eq!(
            structured_log_level_in_record(&object),
            Some(StructuredLogLevel::Error)
        );
    }

    #[test]
    fn sanitize_text_escapes_controls() {
        assert_eq!(sanitize_text("a\n\u{1b}"), "a\\n\\u001b");
    }
}
