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

        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| eyre::eyre!("invalid env file line {line_no}: missing '='"))?;

        let key = key.trim();
        if key.is_empty() {
            return Err(eyre::eyre!("invalid env file line {line_no}: empty key"));
        }

        let mut value = value.trim().to_string();
        if value.len() >= 2 {
            let bytes = value.as_bytes();
            let first = bytes[0];
            let last = bytes[bytes.len() - 1];
            if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
                value = value[1..value.len() - 1].to_string();
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
    for (k, v) in env.iter() {
        current.insert(k.clone(), v.clone());
    }

    let mut out = env.clone();

    for _ in 0..8 {
        let mut changed = false;
        let mut new_map = EnvMap::new();
        for (k, v) in out.iter() {
            let expanded = interpolate(v, &current);
            if expanded != *v {
                changed = true;
            }
            new_map.insert(k.clone(), expanded.clone());
            current.insert(k.clone(), expanded);
        }
        out = new_map;
        if !changed {
            break;
        }
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
            while let Some(c) = chars.next() {
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
            key.push(chars.next().unwrap());
            while let Some(c) = chars.peek().copied() {
                if !is_var_continue(c) {
                    break;
                }
                key.push(chars.next().unwrap());
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
}
