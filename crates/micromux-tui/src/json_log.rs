use micromux::{StructuredLogLevel, is_structured_log_level_key};
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
const MESSAGE_KEYS: &[&str] = &["message", "msg"];
const FIELDS_KEY: &str = "fields";

#[must_use]
pub(crate) fn format_line(line: &str, pretty_json: bool) -> String {
    if !pretty_json {
        return line.to_string();
    }
    format_json_line(line).unwrap_or_else(|| line.to_string())
}

fn format_json_line(line: &str) -> Option<String> {
    let value = parse_json_value(line)?;
    let Value::Object(object) = value else {
        return None;
    };
    let formatted = format_object(&object);
    (!formatted.is_empty()).then_some(formatted)
}

fn parse_json_value(line: &str) -> Option<Value> {
    let trimmed = line.trim_start();
    serde_json::from_str::<Value>(trimmed).ok().or_else(|| {
        trimmed.contains('\x1b').then(|| {
            let stripped = strip_ansi_escapes::strip_str(trimmed);
            serde_json::from_str::<Value>(stripped.trim_start()).ok()
        })?
    })
}

fn format_object(object: &Map<String, Value>) -> String {
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

    for (key, value) in object {
        if is_structured_log_level_key(key) || key_matches(key, MESSAGE_KEYS) {
            continue;
        }
        if key.eq_ignore_ascii_case(FIELDS_KEY)
            && let Value::Object(fields) = value
        {
            append_fields_object(&mut out, fields);
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

fn append_fields_object(out: &mut String, fields: &Map<String, Value>) {
    for (key, value) in fields {
        if is_structured_log_level_key(key) || key_matches(key, MESSAGE_KEYS) {
            continue;
        }
        if !out.is_empty() && !out.ends_with(' ') {
            out.push(' ');
        }
        append_key_value(out, key, value);
    }
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

fn find_level_key(object: &Map<String, Value>) -> Option<(&str, &Value)> {
    object
        .iter()
        .find(|(key, _)| is_structured_log_level_key(key))
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

fn key_matches(key: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| key.eq_ignore_ascii_case(candidate))
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

fn render_scalar(value: &Value) -> String {
    match value {
        Value::String(text) => sanitize_text(text),
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
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
    use super::format_line;
    use similar_asserts::assert_eq;

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
    fn tracing_fields_message_is_promoted_and_other_fields_are_flattened() {
        let line = r#"{"timestamp":"2026-07-01T17:28:02Z","fields":{"severity":"INFO","message":"setup tracer","name":"airtype_api_service"},"filename":"trace.rs","line_number":379,"target":"telemetry::trace"}"#;

        let out = format_line(line, true);

        assert!(out.contains("\x1b[32m[  INFO]\x1b[0m \x1b[37msetup tracer\x1b[0m"));
        assert!(!out.contains("fields="));
        assert!(!out.contains("message="));
        assert!(!out.contains("severity="));
        assert!(out.contains("\x1b[34mname\x1b[0m=\x1b[90mairtype_api_service\x1b[0m"));
        assert!(out.contains("\x1b[34mfilename\x1b[0m=\x1b[90mtrace.rs\x1b[0m"));
        assert!(out.contains("\x1b[34mline_number\x1b[0m=\x1b[90m379\x1b[0m"));
        assert!(out.contains("\x1b[34mtarget\x1b[0m=\x1b[90mtelemetry::trace\x1b[0m"));
        assert!(out.contains("\x1b[34mtimestamp\x1b[0m=\x1b[90m2026-07-01T17:28:02Z\x1b[0m"));
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
}
