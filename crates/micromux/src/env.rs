use color_eyre::eyre;
use indexmap::IndexMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvMap {
    inner: IndexMap<String, String>,
}

pub fn interpolate_str(input: &str, env: &HashMap<String, String>) -> String {
    interpolate(input, env)
}

impl Default for EnvMap {
    fn default() -> Self {
        Self::new()
    }
}

impl EnvMap {
    pub fn new() -> Self {
        Self {
            inner: IndexMap::new(),
        }
    }

    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.inner.insert(key.into(), value.into());
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.inner.iter()
    }

    pub fn extend(&mut self, other: EnvMap) {
        self.inner.extend(other.inner);
    }
}

fn strip_export_prefix(line: &str) -> &str {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed
        .strip_prefix("export")
        .and_then(|s| s.strip_prefix(char::is_whitespace))
    {
        rest.trim_start()
    } else {
        trimmed
    }
}

/// Strip a trailing inline comment from a dotenv *value* (the part after the first `=`).
///
/// The key/value split happens before this is called, so a stray quote in an unrelated part of
/// the line can no longer flip the parser into a never-closed "quoted" state that swallows the
/// comment marker. If the value begins with a quote, the comment (if any) starts after the
/// matching close quote; otherwise the comment starts at the first `#` preceded by whitespace.
fn strip_value_inline_comment(value: &str) -> &str {
    let trimmed = value.trim_start();
    let quote = match trimmed.chars().next() {
        Some(c @ ('\'' | '"')) => Some(c),
        _ => None,
    };

    if let Some(quote) = quote {
        let mut chars = trimmed.char_indices();
        let _ = chars.next(); // skip the opening quote
        let mut escaped = false;
        for (i, c) in chars {
            if escaped {
                // The previous char was a backslash escape (double-quoted values only).
                escaped = false;
                continue;
            }
            if quote == '"' && c == '\\' {
                escaped = true;
                continue;
            }
            if c == quote {
                // Value runs up to and including the closing quote; the rest is a comment.
                return trimmed.get(..=i).unwrap_or(trimmed);
            }
        }
        // No closing quote: treat the whole thing as the value.
        trimmed
    } else {
        let mut prev_was_ws = false;
        for (i, ch) in trimmed.char_indices() {
            if ch == '#' && prev_was_ws {
                return trimmed.get(..i).unwrap_or(trimmed).trim_end();
            }
            prev_was_ws = ch.is_whitespace();
        }
        trimmed
    }
}

fn unescape_double_quoted_value(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') | None => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

fn parse_value(raw_value: &str) -> String {
    let value = raw_value.trim().to_string();
    if value.len() < 2 {
        return value;
    }

    if let Some(inner) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return unescape_double_quoted_value(inner);
    }

    if let Some(inner) = value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        // Single quotes are fully literal (POSIX/dotenv semantics): no interpolation. Values
        // always pass through `interpolate` later, so escape every `$` as `$$` (which
        // `interpolate` collapses back to a single `$`) to keep the literal intact.
        return inner.replace('$', "$$");
    }

    value
}

pub fn parse_dotenv(contents: &str) -> eyre::Result<EnvMap> {
    let mut env = EnvMap::new();

    for (idx, raw_line) in contents.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = strip_export_prefix(line);

        let (key, raw_value) = line
            .split_once('=')
            .ok_or_else(|| eyre::eyre!("invalid env file line {line_no}: missing '='"))?;

        let key = key.trim();
        if key.is_empty() {
            return Err(eyre::eyre!("invalid env file line {line_no}: empty key"));
        }

        let value = parse_value(strip_value_inline_comment(raw_value));

        env.insert(key.to_string(), value);
    }

    Ok(env)
}

pub fn load_env_files_sync(paths: &[PathBuf]) -> eyre::Result<EnvMap> {
    let mut env = EnvMap::new();
    for path in paths {
        let content = std::fs::read_to_string(path)
            .map_err(|err| eyre::eyre!("failed to read env file {}: {err}", path.display()))?;
        let parsed = parse_dotenv(&content)
            .map_err(|err| eyre::eyre!("failed to parse env file {}: {err}", path.display()))?;
        env.extend(parsed);
    }
    Ok(env)
}

pub fn expand_env_values(env: &EnvMap, base: &HashMap<String, String>) -> EnvMap {
    let mut current: HashMap<String, String> = base.clone();
    let mut out = EnvMap::new();

    for (k, v) in env.iter() {
        let expanded = interpolate(v, &current);
        out.insert(k.clone(), expanded.clone());
        current.insert(k.clone(), expanded);
    }

    out
}

pub fn resolve_path(config_dir: &Path, raw: &str) -> eyre::Result<PathBuf> {
    let expanded = shellexpand::full(raw)
        .map_err(|err| eyre::eyre!("failed to expand path `{raw}`: {err}"))?
        .to_string();
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(config_dir.join(path))
    }
}

fn interpolate(input: &str, env: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '$' {
            out.push(ch);
            continue;
        }

        let Some(next) = chars.peek().copied() else {
            out.push('$');
            break;
        };

        if next == '$' {
            let _ = chars.next();
            out.push('$');
            continue;
        }

        if next == '{' {
            let _ = chars.next();
            let mut key = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                key.push(c);
            }
            if let Some(value) = env.get(&key) {
                out.push_str(value);
            }
            continue;
        }

        if is_var_start(next) {
            let mut key = String::new();
            let Some(first) = chars.next() else {
                out.push('$');
                continue;
            };
            key.push(first);
            while let Some(c) = chars.peek().copied() {
                if !is_var_continue(c) {
                    break;
                }
                let Some(next) = chars.next() else {
                    break;
                };
                key.push(next);
            }
            if let Some(value) = env.get(&key) {
                out.push_str(value);
            }
            continue;
        }

        out.push('$');
    }

    out
}

fn is_var_start(c: char) -> bool {
    c == '_' || c.is_ascii_alphabetic()
}

fn is_var_continue(c: char) -> bool {
    is_var_start(c) || c.is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_parse_basic() -> eyre::Result<()> {
        let env = parse_dotenv("FOO=bar\n# comment\nexport BAZ=qux\n")?;
        assert_eq!(env.inner.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(env.inner.get("BAZ").map(String::as_str), Some("qux"));
        Ok(())
    }

    #[test]
    fn interpolate_vars() {
        let mut m = HashMap::new();
        m.insert("A".to_string(), "x".to_string());
        m.insert("B".to_string(), "y".to_string());
        assert_eq!(interpolate("$A-$B", &m), "x-y");
        assert_eq!(interpolate("${A}${B}", &m), "xy");
        assert_eq!(interpolate("$$A", &m), "$A");
    }

    #[test]
    fn expand_env_values_is_single_pass_and_ordered() {
        let mut base = HashMap::new();
        base.insert("X".to_string(), "base".to_string());

        let mut env = EnvMap::new();
        env.insert("A", "${X}-a");
        env.insert("B", "${A}-b");

        let out = expand_env_values(&env, &base);
        assert_eq!(out.inner.get("A").map(String::as_str), Some("base-a"));
        assert_eq!(out.inner.get("B").map(String::as_str), Some("base-a-b"));
    }

    #[test]
    fn expand_env_values_does_not_expand_forward_references() {
        let base = HashMap::new();

        let mut env = EnvMap::new();
        env.insert("B", "${A}-b");
        env.insert("A", "a");

        let out = expand_env_values(&env, &base);
        assert_eq!(out.inner.get("B").map(String::as_str), Some("-b"));
        assert_eq!(out.inner.get("A").map(String::as_str), Some("a"));
    }

    #[test]
    fn dotenv_allows_export_with_extra_whitespace() -> eyre::Result<()> {
        let env = parse_dotenv("export   FOO=bar\nexport\tBAZ=qux\n")?;
        assert_eq!(env.inner.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(env.inner.get("BAZ").map(String::as_str), Some("qux"));
        Ok(())
    }

    #[test]
    fn dotenv_strips_inline_comments_outside_quotes() -> eyre::Result<()> {
        let env = parse_dotenv("FOO=bar # comment\nBAR=\"x # y\" # z\n")?;
        assert_eq!(env.inner.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(env.inner.get("BAR").map(String::as_str), Some("x # y"));
        Ok(())
    }

    #[test]
    fn dotenv_double_quote_unescapes_common_sequences() -> eyre::Result<()> {
        let env = parse_dotenv("A=\"x\\n\\\"y\\\"\\\\z\"\n")?;
        assert_eq!(env.inner.get("A").map(String::as_str), Some("x\n\"y\"\\z"));
        Ok(())
    }

    #[test]
    fn single_quoted_values_are_literal_after_expansion() -> eyre::Result<()> {
        let env = parse_dotenv("PASS='s3cr$t!'\nLIT='${X}'\nDOLLARS='$$'\n")?;
        let out = expand_env_values(&env, &HashMap::new());
        assert_eq!(out.inner.get("PASS").map(String::as_str), Some("s3cr$t!"));
        assert_eq!(out.inner.get("LIT").map(String::as_str), Some("${X}"));
        assert_eq!(out.inner.get("DOLLARS").map(String::as_str), Some("$$"));
        Ok(())
    }

    #[test]
    fn double_quoted_values_still_interpolate() -> eyre::Result<()> {
        let mut base = HashMap::new();
        base.insert("X".to_string(), "world".to_string());
        let env = parse_dotenv("GREETING=\"hello ${X}\"\n")?;
        let out = expand_env_values(&env, &base);
        assert_eq!(
            out.inner.get("GREETING").map(String::as_str),
            Some("hello world")
        );
        Ok(())
    }

    #[test]
    fn dotenv_stray_quote_does_not_disable_comment_stripping() -> eyre::Result<()> {
        let env = parse_dotenv("FOO=don't # comment\nPRICE=5\" # usd\n")?;
        assert_eq!(env.inner.get("FOO").map(String::as_str), Some("don't"));
        assert_eq!(env.inner.get("PRICE").map(String::as_str), Some("5\""));
        Ok(())
    }
}
