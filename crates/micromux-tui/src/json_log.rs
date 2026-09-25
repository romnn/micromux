use micromux::{
    FIELDS_KEY, LogLine, MESSAGE_KEYS, RecordTimestamp, StructuredLogLevel, find_fields_object,
    find_key, is_structured_log_level_key, key_matches, render_scalar, sanitize_text,
    structured_log_level_in_record, structured_log_timestamp_in_record,
};
use serde_json::{Map, Value};

const RESET: &str = "\x1b[0m";
const WHITE: &str = "\x1b[37m";
const GRAY: &str = "\x1b[90m";
const BLUE: &str = "\x1b[34m";
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const MAGENTA: &str = "\x1b[35m";

const LEVEL_LABEL_WIDTH: usize = 6;

/// How the log pane renders one service's records.
///
/// The TUI rebuilds a service's cached lines whenever this changes, so it carries every input
/// that affects the rendered text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LineFormat {
    /// Render structured JSON records as compact colored lines instead of raw JSON.
    pub pretty_json: bool,
    /// Prefix structured JSON records with their timestamp in local time.
    pub timestamps: bool,
    /// Hide structured records less severe than this level; `None` shows every level.
    pub min_level: Option<StructuredLogLevel>,
    /// Field keys omitted from pretty-printed records.
    pub hidden_fields: Vec<String>,
}

impl LineFormat {
    fn hides(&self, key: &str) -> bool {
        self.hidden_fields.iter().any(|hidden| hidden == key)
    }
}

/// Render one record for the log pane, or `None` when the level threshold hides it.
///
/// Only structured JSON records are filtered and timestamped.
/// Plain lines such as build output, panics, and stack traces pass through unchanged, so a level
/// threshold never hides the output that explains a crash.
#[must_use]
pub(crate) fn format_record(record: &LogLine, format: &LineFormat) -> Option<String> {
    let object = if format.pretty_json || format.timestamps || format.min_level.is_some() {
        parse_json_object(&record.line)
    } else {
        None
    };
    let Some(object) = object else {
        return Some(record.line.clone());
    };
    if let Some(min_level) = format.min_level
        && structured_log_level_in_record(&object).is_some_and(|level| level < min_level)
    {
        return None;
    }

    let timestamp = structured_log_timestamp_in_record(&object);
    let mut out = String::new();
    if format.timestamps {
        // Records without their own timestamp fall back to when micromux ingested them.
        let unix_ms = timestamp.map_or(record.timestamp_unix_ms, |timestamp| timestamp.unix_ms);
        append_timestamp(&mut out, unix_ms);
    }
    let body = format
        .pretty_json
        .then(|| format_object(&object, format, timestamp))
        .filter(|body| !body.is_empty());
    out.push_str(body.as_deref().unwrap_or(&record.line));
    Some(out)
}

fn parse_json_object(line: &str) -> Option<Map<String, Value>> {
    let trimmed = line.trim_start();
    let value = serde_json::from_str::<Value>(trimmed).ok().or_else(|| {
        trimmed.contains('\x1b').then(|| {
            let stripped = strip_ansi_escapes::strip_str(trimmed);
            serde_json::from_str::<Value>(stripped.trim_start()).ok()
        })?
    })?;
    match value {
        Value::Object(object) => Some(object),
        _ => None,
    }
}

fn append_timestamp(out: &mut String, unix_ms: u64) {
    // Peers that predate ingestion timestamps report zero, which is no time worth showing.
    let Some(timestamp) = i64::try_from(unix_ms)
        .ok()
        .filter(|unix_ms| *unix_ms > 0)
        .and_then(chrono::DateTime::from_timestamp_millis)
    else {
        return;
    };
    let local = timestamp.with_timezone(&chrono::Local);
    out.push_str(GRAY);
    out.push_str(&local.format("%H:%M:%S%.3f").to_string());
    out.push_str(RESET);
    out.push(' ');
}

fn format_object(
    object: &Map<String, Value>,
    format: &LineFormat,
    timestamp: Option<RecordTimestamp<'_>>,
) -> String {
    let fields = find_fields_object(object);
    let level = find_level_key(object).or_else(|| fields.and_then(find_level_key));
    let message = find_key(object, MESSAGE_KEYS)
        .or_else(|| fields.and_then(|fields| find_key(fields, MESSAGE_KEYS)));
    let mut out = String::new();

    if let Some((_key, value)) = level {
        let rendered = level_label(value);
        out.push_str(level_color(value));
        out.push('[');
        append_padded_level(&mut out, &rendered);
        out.push(']');
        out.push_str(RESET);
        out.push(' ');
    }

    if let Some((_key, value)) = message {
        out.push_str(WHITE);
        out.push_str(&render_scalar(value));
        out.push_str(RESET);
    }

    // The timestamp either leads the line or is switched off, so it never repeats as a field.
    let top_level_timestamp_key = timestamp
        .filter(|timestamp| !timestamp.nested)
        .map(|timestamp| timestamp.key);
    let nested_timestamp_key = timestamp
        .filter(|timestamp| timestamp.nested)
        .map(|timestamp| timestamp.key);
    for (key, value) in object {
        if is_structured_log_level_key(key)
            || key_matches(key, MESSAGE_KEYS)
            || top_level_timestamp_key == Some(key.as_str())
            || format.hides(key)
        {
            continue;
        }
        if key.eq_ignore_ascii_case(FIELDS_KEY)
            && let Value::Object(fields) = value
        {
            append_fields_object(&mut out, fields, format, nested_timestamp_key);
            continue;
        }
        if !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
        append_key_value(&mut out, key, value);
    }

    if out.ends_with(' ') {
        out.pop();
    }
    out
}

fn append_fields_object(
    out: &mut String,
    fields: &Map<String, Value>,
    format: &LineFormat,
    timestamp_key: Option<&str>,
) {
    for (key, value) in fields {
        if is_structured_log_level_key(key)
            || key_matches(key, MESSAGE_KEYS)
            || timestamp_key == Some(key.as_str())
            || format.hides(key)
        {
            continue;
        }
        if !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
        append_key_value(out, key, value);
    }
}

fn find_level_key(object: &Map<String, Value>) -> Option<(&str, &Value)> {
    object
        .iter()
        .find(|(key, _)| is_structured_log_level_key(key))
        .map(|(key, value)| (key.as_str(), value))
}

fn append_key_value(out: &mut String, key: &str, value: &Value) {
    out.push_str(BLUE);
    out.push_str(&sanitize_text(key));
    out.push_str(RESET);
    out.push('=');
    out.push_str(GRAY);
    out.push_str(&render_scalar(value));
    out.push_str(RESET);
}

fn level_label(value: &Value) -> String {
    StructuredLogLevel::from_value(value)
        .map(StructuredLogLevel::canonical)
        .map(str::to_string)
        .unwrap_or_else(|| render_scalar(value))
        .to_ascii_uppercase()
}

fn append_padded_level(out: &mut String, level: &str) {
    let padding = LEVEL_LABEL_WIDTH.saturating_sub(level.len());
    out.extend(std::iter::repeat_n(' ', padding));
    out.push_str(level);
}

fn level_color(value: &Value) -> &'static str {
    StructuredLogLevel::from_value(value).map_or(BLUE, level_color_name)
}

fn level_color_name(level: StructuredLogLevel) -> &'static str {
    match level {
        StructuredLogLevel::Trace => GRAY,
        StructuredLogLevel::Debug => CYAN,
        StructuredLogLevel::Info => GREEN,
        StructuredLogLevel::Warn => YELLOW,
        StructuredLogLevel::Error => RED,
        StructuredLogLevel::Fatal => MAGENTA,
    }
}

#[cfg(test)]
mod tests {
    use super::{LineFormat, format_record};
    use micromux::{LogLine, StructuredLogLevel};
    use similar_asserts::assert_eq;

    /// Ingestion time used by records under test, distinct from any record's own timestamp.
    const INGESTED_AT_UNIX_MS: u64 = 1_790_000_000_000;

    fn record(line: &str) -> LogLine {
        LogLine {
            seq: 1,
            run_generation: 1,
            timestamp_unix_ms: INGESTED_AT_UNIX_MS,
            line: line.to_string(),
        }
    }

    fn plain_format(pretty_json: bool) -> LineFormat {
        LineFormat {
            pretty_json,
            timestamps: false,
            min_level: None,
            hidden_fields: Vec::new(),
        }
    }

    /// Render with no timestamps, level threshold, or hidden fields, none of which can drop a
    /// record.
    fn format_line(line: &str, pretty_json: bool) -> String {
        format_record(&record(line), &plain_format(pretty_json)).unwrap_or_default()
    }

    fn local_time_label(unix_ms: u64) -> String {
        let timestamp = i64::try_from(unix_ms)
            .ok()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .unwrap_or_default()
            .with_timezone(&chrono::Local);
        format!("\x1b[90m{}\x1b[0m ", timestamp.format("%H:%M:%S%.3f"))
    }

    #[test]
    fn non_json_lines_pass_through() {
        assert_eq!(format_line("plain", true), "plain");
    }

    #[test]
    fn pretty_prints_level_message_and_fields() {
        let line = r#"{"level":"warn","msg":"slow request","path":"/api","elapsed_ms":42}"#;

        let out = format_line(line, true);

        assert!(out.contains("\x1b[33m[  WARN]\x1b[0m"));
        assert!(out.contains("\x1b[37mslow request\x1b[0m"));
        assert!(!out.contains("level"));
        assert!(out.contains("\x1b[34mpath\x1b[0m=\x1b[90m/api\x1b[0m"));
        assert!(out.contains("\x1b[34melapsed_ms\x1b[0m=\x1b[90m42\x1b[0m"));
    }

    #[test]
    fn numeric_levels_are_named_and_colored() {
        let out = format_line(r#"{"level":50,"message":"failed"}"#, true);

        assert!(out.contains("\x1b[31m[ ERROR]\x1b[0m"));
    }

    #[test]
    fn escaped_text_stays_on_one_tui_row() {
        let out = format_line(r#"{"level":"info","message":"hello\nworld"}"#, true);

        assert!(out.contains("hello\\nworld"));
    }

    #[test]
    fn escaped_terminal_controls_do_not_become_ansi() {
        let out = format_line(r#"{"level":"info","message":"bad\u001b[31m"}"#, true);

        assert!(out.contains("bad\\u001b[31m"));
        assert!(!out.contains("bad\x1b[31m"));
    }

    #[test]
    fn escaped_key_controls_do_not_become_ansi() {
        let out = format_line(
            r#"{"level":"info","message":"safe","bad\u001b[31m":"x"}"#,
            true,
        );

        assert!(out.contains("bad\\u001b[31m"));
        assert!(!out.contains("bad\x1b[31m"));
    }

    #[test]
    fn ansi_wrapped_json_is_still_pretty_printed() {
        let out = format_line(
            "\x1b[2m{\"level\":\"error\",\"msg\":\"failed\"}\x1b[0m",
            true,
        );

        assert!(out.contains("\x1b[31m[ ERROR]\x1b[0m"));
        assert!(out.contains("\x1b[37mfailed\x1b[0m"));
    }

    #[test]
    fn non_json_build_output_does_not_disable_later_json_pretty_printing() {
        let lines = [
            "   Compiling api-service v0.1.0",
            r#"{"level":"info","message":"server ready","target":"api"}"#,
        ];

        let rendered = lines
            .into_iter()
            .map(|line| format_line(line, true))
            .collect::<Vec<_>>();

        assert_eq!(rendered[0], "   Compiling api-service v0.1.0");
        assert!(rendered[1].contains("\x1b[32m[  INFO]\x1b[0m"));
        assert!(rendered[1].contains("\x1b[37mserver ready\x1b[0m"));
        assert!(rendered[1].contains("\x1b[34mtarget\x1b[0m=\x1b[90mapi\x1b[0m"));
    }

    #[test]
    fn multiline_non_json_output_stays_raw() {
        let line = "Error:\n   0: failed";

        assert_eq!(format_line(line, true), line);
    }

    #[test]
    fn tracing_fields_message_is_promoted_and_other_fields_are_flattened() {
        let line = r#"{"timestamp":"2026-07-01T17:28:02Z","fields":{"severity":"INFO","message":"setup tracer","name":"demo_api_service"},"filename":"trace.rs","line_number":379,"target":"telemetry::trace"}"#;

        let out = format_line(line, true);

        assert!(out.contains("\x1b[32m[  INFO]\x1b[0m \x1b[37msetup tracer\x1b[0m"));
        assert!(!out.contains("fields="));
        assert!(!out.contains("message="));
        assert!(!out.contains("severity="));
        assert!(out.contains("\x1b[34mname\x1b[0m=\x1b[90mdemo_api_service\x1b[0m"));
        assert!(out.contains("\x1b[34mfilename\x1b[0m=\x1b[90mtrace.rs\x1b[0m"));
        assert!(out.contains("\x1b[34mline_number\x1b[0m=\x1b[90m379\x1b[0m"));
        assert!(out.contains("\x1b[34mtarget\x1b[0m=\x1b[90mtelemetry::trace\x1b[0m"));
        // The record timestamp belongs to the optional prefix, never to the trailing fields.
        assert!(!out.contains("timestamp="));
    }

    #[test]
    fn empty_objects_fall_back_to_raw_json() {
        assert_eq!(format_line("{}", true), "{}");
    }

    #[test]
    fn disabled_pretty_printing_returns_original_json() {
        let line = r#"{"level":"info","message":"hello"}"#;

        assert_eq!(format_line(line, false), line);
    }

    #[test]
    fn timestamps_lead_structured_records_in_local_time() {
        let format = LineFormat {
            timestamps: true,
            ..plain_format(true)
        };
        let own_timestamp_unix_ms = 1_790_342_949_148;

        let own = format_record(
            &record(
                r#"{"timestamp":"2026-09-25T13:29:09.148391Z","level":"DEBUG","fields":{"message":"tick"}}"#,
            ),
            &format,
        );
        let fallback = format_record(&record(r#"{"level":"info","msg":"no clock"}"#), &format);

        // A record's own timestamp wins and is not repeated as a trailing field.
        assert_eq!(
            own,
            Some(format!(
                "{}\x1b[36m[ DEBUG]\x1b[0m \x1b[37mtick\x1b[0m",
                local_time_label(own_timestamp_unix_ms)
            ))
        );
        // A record without one shows when micromux ingested it.
        assert_eq!(
            fallback,
            Some(format!(
                "{}\x1b[32m[  INFO]\x1b[0m \x1b[37mno clock\x1b[0m",
                local_time_label(INGESTED_AT_UNIX_MS)
            ))
        );
    }

    #[test]
    fn timestamps_also_prefix_raw_json_but_never_plain_lines() {
        let format = LineFormat {
            timestamps: true,
            ..plain_format(false)
        };
        let json = r#"{"level":"info","msg":"raw"}"#;

        assert_eq!(
            format_record(&record(json), &format),
            Some(format!("{}{json}", local_time_label(INGESTED_AT_UNIX_MS)))
        );
        assert_eq!(
            format_record(&record("   Compiling api v0.1.0"), &format),
            Some("   Compiling api v0.1.0".to_string())
        );
    }

    #[test]
    fn nested_field_timestamps_are_dropped_from_the_fields() {
        let out = format_line(
            r#"{"fields":{"level":"info","message":"nested","ts":1790342949,"id":7}}"#,
            true,
        );

        assert_eq!(
            out,
            "\x1b[32m[  INFO]\x1b[0m \x1b[37mnested\x1b[0m \x1b[34mid\x1b[0m=\x1b[90m7\x1b[0m"
        );
    }

    #[test]
    fn level_threshold_hides_only_less_severe_structured_records() {
        let format = LineFormat {
            min_level: Some(StructuredLogLevel::Info),
            ..plain_format(true)
        };
        let shown = |line: &str| format_record(&record(line), &format).is_some();

        // Structured records below the threshold are hidden.
        assert!(!shown(r#"{"level":"trace","msg":"noise"}"#));
        assert!(!shown(r#"{"fields":{"level":"debug","message":"nested"}}"#));
        // Records at or above it stay.
        assert!(shown(r#"{"level":"info","msg":"ready"}"#));
        assert!(shown(r#"{"level":50,"msg":"failed"}"#));
        // Without a recognizable level there is nothing to compare, so the line stays visible.
        assert!(shown(r#"{"msg":"levelless"}"#));
        assert!(shown("thread 'main' panicked at src/main.rs:1:1"));
        // The threshold applies even when JSON is shown raw.
        let raw = LineFormat {
            pretty_json: false,
            ..format.clone()
        };
        assert_eq!(
            format_record(&record(r#"{"level":"debug","msg":"raw"}"#), &raw),
            None
        );
    }

    #[test]
    fn hidden_fields_are_omitted_at_the_top_level_and_in_nested_fields() {
        let format = LineFormat {
            hidden_fields: vec![
                "filename".to_string(),
                "span".to_string(),
                "trace_id".to_string(),
            ],
            ..plain_format(true)
        };
        let line = r#"{"level":"DEBUG","fields":{"message":"request completed","latency":"0 ms","trace_id":"abc"},"target":"http","filename":"server.rs","span":{"name":"handle_request"}}"#;

        let out = format_record(&record(line), &format);

        // Only the configured keys disappear; the message, level, and other fields remain.
        assert_eq!(
            out,
            Some(
                concat!(
                    "\x1b[36m[ DEBUG]\x1b[0m \x1b[37mrequest completed\x1b[0m ",
                    "\x1b[34mlatency\x1b[0m=\x1b[90m0 ms\x1b[0m ",
                    "\x1b[34mtarget\x1b[0m=\x1b[90mhttp\x1b[0m",
                )
                .to_string()
            )
        );
    }
}
