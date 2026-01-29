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

    pub fn get(&self, key: &str) -> Option<&str> {
        self.inner.get(key).map(|s| s.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.inner.iter()
    }

    pub fn extend(&mut self, other: EnvMap) {
        self.inner.extend(other.inner);
    }

    pub fn into_inner(self) -> IndexMap<String, String> {
        self.inner
    }
}

pub fn parse_dotenv(contents: &str) -> eyre::Result<EnvMap> {
    let mut env = EnvMap::new();

    for (idx, raw_line) in contents.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = {
            let trimmed = line.trim_start();
            trimmed
                .strip_prefix("export")
                .and_then(|s| s.strip_prefix(char::is_whitespace))
                .map(|rest| rest.trim_start())
                .unwrap_or(trimmed)
        };

        let mut in_single = false;
        let mut in_double = false;
        let mut last_was_ws = false;
        let mut cleaned = String::with_capacity(line.len());
        let chars = line.chars().peekable();
        for ch in chars {
            match ch {
                '\'' if !in_double => {
                    in_single = !in_single;
                    cleaned.push(ch);
                    last_was_ws = false;
                }
                '"' if !in_single => {
                    in_double = !in_double;
                    cleaned.push(ch);
                    last_was_ws = false;
                }
                '#' if !in_single && !in_double && last_was_ws => {
                    while cleaned.ends_with(char::is_whitespace) {
                        cleaned.pop();
                    }
                    break;
                }
                _ => {
                    last_was_ws = ch.is_whitespace();
                    cleaned.push(ch);
                }
            }
        }
        let line = cleaned.trim();

        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| eyre::eyre!("invalid env file line {line_no}: missing '='"))?;

        let key = key.trim();
        if key.is_empty() {
            return Err(eyre::eyre!("invalid env file line {line_no}: empty key"));
        }

        let mut value = value.trim().to_string();
        if value.len() >= 2 {
            if value
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .is_some()
            {
                let inner = value
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .unwrap_or("");
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
                        Some('\\') => out.push('\\'),
                        Some(other) => {
                            out.push('\\');
                            out.push(other);
                        }
                        None => out.push('\\'),
                    }
                }
                value = out;
            } else if value
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
                .is_some()
            {
                value = value
                    .strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''))
                    .unwrap_or("")
                    .to_string();
            }
        }

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

pub async fn load_env_files(paths: &[PathBuf]) -> eyre::Result<EnvMap> {
    let mut env = EnvMap::new();
    for path in paths {
        let content = tokio::fs::read_to_string(path)
            .await
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
        assert_eq!(env.get("FOO"), Some("bar"));
        assert_eq!(env.get("BAZ"), Some("qux"));
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
        assert_eq!(out.get("A"), Some("base-a"));
        assert_eq!(out.get("B"), Some("base-a-b"));
    }

    #[test]
    fn expand_env_values_does_not_expand_forward_references() {
        let base = HashMap::new();

        let mut env = EnvMap::new();
        env.insert("B", "${A}-b");
        env.insert("A", "a");

        let out = expand_env_values(&env, &base);
        assert_eq!(out.get("B"), Some("-b"));
        assert_eq!(out.get("A"), Some("a"));
    }

    #[test]
    fn dotenv_allows_export_with_extra_whitespace() -> eyre::Result<()> {
        let env = parse_dotenv("export   FOO=bar\nexport\tBAZ=qux\n")?;
        assert_eq!(env.get("FOO"), Some("bar"));
        assert_eq!(env.get("BAZ"), Some("qux"));
        Ok(())
    }

    #[test]
    fn dotenv_strips_inline_comments_outside_quotes() -> eyre::Result<()> {
        let env = parse_dotenv("FOO=bar # comment\nBAR=\"x # y\" # z\n")?;
        assert_eq!(env.get("FOO"), Some("bar"));
        assert_eq!(env.get("BAR"), Some("x # y"));
        Ok(())
    }

    #[test]
    fn dotenv_double_quote_unescapes_common_sequences() -> eyre::Result<()> {
        let env = parse_dotenv("A=\"x\\n\\\"y\\\"\\\\z\"\n")?;
        assert_eq!(env.get("A"), Some("x\n\"y\"\\z"));
        Ok(())
    }
}
