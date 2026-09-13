//! Dotenv loading and `${VAR}` interpolation for configured values.
//!
//! [`interpolate`] is the single substitution primitive behind every configured value that
//! accepts a variable reference.
//! It knows nothing about which field a value came from; [`crate::service`] owns that mapping
//! and decides which environment each field sees.

use indexmap::IndexMap;
use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

const MAX_ENV_FILE_BYTES: usize = 4 * 1024 * 1024;

/// Deepest `${A:-${B:-…}}` nesting accepted inside a default value.
///
/// Defaults are interpolated recursively, so an adversarial value could otherwise recurse as
/// deep as the 4 MiB config limit allows.
const MAX_DEFAULT_NESTING: usize = 16;

/// Errors from reading and parsing environment files.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A dotenv line does not contain a key-value separator.
    #[error("invalid env file line {line}: missing '='")]
    MissingSeparator {
        /// One-based line number.
        line: usize,
    },
    /// A dotenv line contains an empty key.
    #[error("invalid env file line {line}: empty key")]
    EmptyKey {
        /// One-based line number.
        line: usize,
    },
    /// An environment file could not be read.
    #[error("failed to read env file {}: {source}", path.display())]
    ReadFile {
        /// Path of the unreadable file.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// An environment file contains invalid dotenv syntax.
    #[error("failed to parse env file {}: {source}", path.display())]
    ParseFile {
        /// Path of the invalid file.
        path: PathBuf,
        /// Underlying dotenv error.
        #[source]
        source: Box<Self>,
    },
    /// An environment file exceeded the supported size.
    #[error(
        "environment file {} exceeds the {MAX_ENV_FILE_BYTES} byte limit",
        path.display()
    )]
    FileTooLarge {
        /// Path of the oversized file.
        path: PathBuf,
    },
}

/// A variable reference in a configured value could not be substituted.
///
/// The variant names the defect precisely so the caller can point the author at the fix.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InterpolationError {
    /// The referenced variable has no value in the environment the value is resolved against.
    #[error("variable `{variable}` is not set")]
    Unset {
        /// Name of the referenced variable.
        variable: String,
    },
    /// A `${` reference has no closing brace.
    #[error("unterminated `${{` reference")]
    Unterminated,
    /// A braced reference is not one of the supported forms.
    #[error("invalid variable reference `${{{reference}}}`")]
    InvalidReference {
        /// Text between the braces.
        reference: String,
    },
    /// A braced reference uses a shell operator micromux does not implement.
    #[error("the `{operator}` operator is not supported; use `:-` or `-` for a default")]
    UnsupportedOperator {
        /// The operator as written, such as `:+` or `:?`.
        operator: String,
    },
    /// Default values nest deeper than the supported limit.
    #[error("variable defaults nest deeper than {MAX_DEFAULT_NESTING} levels")]
    NestingTooDeep,
}

/// A value inside an environment map could not be interpolated.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("value of `{key}`")]
pub struct ValueError {
    /// Key whose value failed to interpolate.
    pub key: String,
    /// The failing reference.
    #[source]
    pub source: InterpolationError,
}

/// Ordered environment entries, as loaded from dotenv files or configured inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvMap {
    inner: IndexMap<String, String>,
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

    pub fn extend(&mut self, other: EnvMap) {
        self.inner.extend(other.inner);
    }
}

impl IntoIterator for EnvMap {
    type Item = (String, String);
    type IntoIter = indexmap::map::IntoIter<String, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.inner.into_iter()
    }
}

/// A dotenv value as written, before variable substitution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawValue {
    /// An unquoted or double-quoted value whose references [`expand_env_values`] substitutes.
    Expandable(String),
    /// A single-quoted value that is taken verbatim.
    Literal(String),
}

impl RawValue {
    /// The value text, whether or not it will be expanded.
    #[cfg(test)]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Expandable(value) | Self::Literal(value) => value,
        }
    }
}

/// Ordered dotenv entries whose references have not been substituted yet.
///
/// Only [`expand_env_values`] turns one into an [`EnvMap`], so a loaded file cannot reach a
/// process with its references unresolved or its quoting undone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawEnvMap {
    inner: IndexMap<String, RawValue>,
}

impl RawEnvMap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: impl Into<String>, value: RawValue) {
        self.inner.insert(key.into(), value);
    }

    pub fn extend(&mut self, other: RawEnvMap) {
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

fn parse_value(raw_value: &str) -> RawValue {
    let value = raw_value.trim().to_string();
    if value.len() < 2 {
        return RawValue::Expandable(value);
    }

    if let Some(inner) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return RawValue::Expandable(unescape_double_quoted_value(inner));
    }

    // Single quotes are fully literal (POSIX/dotenv semantics): no interpolation.
    if let Some(inner) = value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return RawValue::Literal(inner.to_string());
    }

    RawValue::Expandable(value)
}

/// Parse dotenv text into its entries, without substituting any variable reference yet.
///
/// # Errors
///
/// Returns [`Error::MissingSeparator`] or [`Error::EmptyKey`] naming the offending line.
pub fn parse_dotenv(contents: &str) -> Result<RawEnvMap, Error> {
    let mut env = RawEnvMap::new();

    for (idx, raw_line) in contents.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = strip_export_prefix(line);

        let (key, raw_value) = line
            .split_once('=')
            .ok_or(Error::MissingSeparator { line: line_no })?;

        let key = key.trim();
        if key.is_empty() {
            return Err(Error::EmptyKey { line: line_no });
        }

        let value = parse_value(strip_value_inline_comment(raw_value));

        env.insert(key.to_string(), value);
    }

    Ok(env)
}

/// Read and parse each dotenv file in order into one unexpanded map, later files overriding
/// earlier ones.
///
/// # Errors
///
/// Returns [`Error::ReadFile`], [`Error::FileTooLarge`], or [`Error::ParseFile`] naming the
/// file.
pub fn load_env_files_sync(paths: &[PathBuf]) -> Result<RawEnvMap, Error> {
    let mut env = RawEnvMap::new();
    for path in paths {
        let file = std::fs::File::open(path).map_err(|source| Error::ReadFile {
            path: path.clone(),
            source,
        })?;
        let mut bytes = Vec::new();
        file.take((MAX_ENV_FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|source| Error::ReadFile {
                path: path.clone(),
                source,
            })?;
        if bytes.len() > MAX_ENV_FILE_BYTES {
            return Err(Error::FileTooLarge { path: path.clone() });
        }
        let content = String::from_utf8(bytes).map_err(|source| Error::ReadFile {
            path: path.clone(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
        })?;
        let parsed = parse_dotenv(&content).map_err(|source| Error::ParseFile {
            path: path.clone(),
            source: Box::new(source),
        })?;
        env.extend(parsed);
    }
    Ok(env)
}

/// Interpolate every expandable value of `env` in order, layered over `base`.
///
/// Each value sees `base` plus the already-expanded entries before it, so a file can build on
/// its own earlier lines but never on later ones.
/// Literal (single-quoted) values are copied through untouched.
///
/// # Errors
///
/// Returns the first value whose reference cannot be substituted.
pub fn expand_env_values(
    env: &RawEnvMap,
    base: &HashMap<String, String>,
) -> Result<EnvMap, ValueError> {
    let mut current: HashMap<String, String> = base.clone();
    let mut out = EnvMap::new();

    for (key, raw) in &env.inner {
        let expanded = match raw {
            RawValue::Literal(value) => value.clone(),
            RawValue::Expandable(value) => {
                interpolate(value, &current).map_err(|source| ValueError {
                    key: key.clone(),
                    source,
                })?
            }
        };
        out.insert(key.clone(), expanded.clone());
        current.insert(key.clone(), expanded);
    }

    Ok(out)
}

/// Resolve a configured path: expand `~`, substitute variables from `env`, and anchor a relative
/// result at `config_dir`.
///
/// # Errors
///
/// Returns the reference that could not be substituted.
pub fn resolve_path(
    config_dir: &Path,
    raw: &str,
    env: &HashMap<String, String>,
) -> Result<PathBuf, InterpolationError> {
    let expanded = interpolate(&shellexpand::tilde(raw), env)?;
    let path = PathBuf::from(expanded);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(config_dir.join(path))
    }
}

/// Substitute variable references in `input` with values from `env`.
///
/// Supported forms are `$VAR`, `${VAR}`, `${VAR:-default}` (default when unset or empty), and
/// `${VAR-default}` (default only when unset).
/// A default may itself contain references.
/// A `$` is only special where a reference starts, directly before `{` or a name; writing `$$`
/// there yields a literal `$` and leaves the reference text alone.
/// Every other `$` (a lone `$$`, `$1`, `$?`, `$(`) is kept as-is so shell syntax in a command
/// passes through untouched.
///
/// # Errors
///
/// Returns [`InterpolationError::Unset`] for a reference without a default whose variable is
/// absent from `env`, and a syntax variant for a malformed reference.
pub fn interpolate(
    input: &str,
    env: &HashMap<String, String>,
) -> Result<String, InterpolationError> {
    interpolate_nested(input, env, 0)
}

fn interpolate_nested(
    input: &str,
    env: &HashMap<String, String>,
    depth: usize,
) -> Result<String, InterpolationError> {
    if depth > MAX_DEFAULT_NESTING {
        return Err(InterpolationError::NestingTooDeep);
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    loop {
        let Some(dollar) = rest.find('$') else {
            out.push_str(rest);
            return Ok(out);
        };
        let (literal, tail) = rest.split_at(dollar);
        out.push_str(literal);
        let after_dollar = tail.strip_prefix('$').unwrap_or(tail);

        // Escaped reference: `$$` directly before a name or `{` yields one literal `$` and
        // leaves the reference text alone.
        // A `$$` anywhere else is not special, so a shell's own `$$` (its pid) survives
        // untouched.
        if let Some(remainder) = after_dollar
            .strip_prefix('$')
            .filter(|remainder| starts_reference(remainder))
        {
            out.push('$');
            rest = remainder;
            continue;
        }

        // Braced reference, possibly with a default
        if let Some(braced) = after_dollar.strip_prefix('{') {
            let (reference, remainder) = split_at_closing_brace(braced)?;
            out.push_str(&resolve_braced_reference(reference, env, depth)?);
            rest = remainder;
            continue;
        }

        // Bare `$NAME`, or a literal `$` when no name follows
        let name_len = variable_name_len(after_dollar);
        if name_len == 0 {
            out.push('$');
            rest = after_dollar;
            continue;
        }
        let (name, remainder) = after_dollar.split_at(name_len);
        out.push_str(lookup(name, env)?);
        rest = remainder;
    }
}

/// Split `input` at the `}` that closes an already-opened `${`, honoring nested braces.
fn split_at_closing_brace(input: &str) -> Result<(&str, &str), InterpolationError> {
    let mut depth = 0usize;
    for (index, ch) in input.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' if depth == 0 => {
                let (reference, tail) = input.split_at(index);
                return Ok((reference, tail.strip_prefix('}').unwrap_or(tail)));
            }
            '}' => depth -= 1,
            _ => {}
        }
    }
    Err(InterpolationError::Unterminated)
}

fn resolve_braced_reference(
    reference: &str,
    env: &HashMap<String, String>,
    depth: usize,
) -> Result<String, InterpolationError> {
    let name_len = variable_name_len(reference);
    let (name, modifier) = reference.split_at(name_len);
    let invalid = || InterpolationError::InvalidReference {
        reference: reference.to_string(),
    };
    if name.is_empty() {
        return Err(invalid());
    }
    if modifier.is_empty() {
        return lookup(name, env).map(str::to_string);
    }
    if let Some(default) = modifier.strip_prefix(":-") {
        return match env.get(name).filter(|value| !value.is_empty()) {
            Some(value) => Ok(value.clone()),
            None => interpolate_nested(default, env, depth + 1),
        };
    }
    if let Some(default) = modifier.strip_prefix('-') {
        return match env.get(name) {
            Some(value) => Ok(value.clone()),
            None => interpolate_nested(default, env, depth + 1),
        };
    }
    // Name the shell operators an author might expect, so the fix is obvious.
    if let Some(operator) = [":+", ":?", ":=", "+", "?", "="]
        .into_iter()
        .find(|operator| modifier.starts_with(operator))
    {
        return Err(InterpolationError::UnsupportedOperator {
            operator: operator.to_string(),
        });
    }
    Err(invalid())
}

fn lookup<'a>(name: &str, env: &'a HashMap<String, String>) -> Result<&'a str, InterpolationError> {
    env.get(name)
        .map(String::as_str)
        .ok_or_else(|| InterpolationError::Unset {
            variable: name.to_string(),
        })
}

/// Whether `input` begins with the text of a reference (`{` or a variable name), so a `$` in
/// front of it would start one.
fn starts_reference(input: &str) -> bool {
    input.starts_with('{') || variable_name_len(input) > 0
}

/// Byte length of the leading `[A-Za-z_][A-Za-z0-9_]*` variable name in `input`, or zero.
fn variable_name_len(input: &str) -> usize {
    let mut chars = input.chars();
    let Some(first) = chars.next().filter(|ch| is_var_start(*ch)) else {
        return 0;
    };
    first.len_utf8()
        + chars
            .take_while(|ch| is_var_continue(*ch))
            .map(char::len_utf8)
            .sum::<usize>()
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
    use color_eyre::eyre;
    use similar_asserts::assert_eq;

    #[test]
    fn dotenv_parse_basic() -> eyre::Result<()> {
        let env = parse_dotenv("FOO=bar\n# comment\nexport BAZ=qux\n")?;
        assert_eq!(env.inner.get("FOO").map(RawValue::as_str), Some("bar"));
        assert_eq!(env.inner.get("BAZ").map(RawValue::as_str), Some("qux"));
        Ok(())
    }

    fn base_environment(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn interpolate_substitutes_bare_and_braced_references() -> eyre::Result<()> {
        let env = base_environment(&[("A", "x"), ("B", "y")]);
        assert_eq!(interpolate("$A-$B", &env)?, "x-y");
        assert_eq!(interpolate("${A}${B}", &env)?, "xy");
        assert_eq!(interpolate("$$A", &env)?, "$A");
        assert_eq!(interpolate("pre${A}post", &env)?, "prexpost");
        Ok(())
    }

    /// Shell syntax that is not a variable reference passes through so a `sh -c` payload keeps
    /// its positional parameters, exit status, pid, and command substitutions.
    #[test]
    fn interpolate_keeps_non_reference_dollars_literal() -> eyre::Result<()> {
        let env = base_environment(&[]);
        for input in [
            "$1",
            "$?",
            "$(date)",
            "$@",
            "cost: 5$",
            "$ ",
            "$",
            "$$",
            "echo $$ > pid; $$$",
        ] {
            assert_eq!(interpolate(input, &env)?, input, "{input}");
        }
        Ok(())
    }

    /// `$$` escapes only where a reference would otherwise start.
    #[test]
    fn dollar_escapes_apply_only_before_references() -> eyre::Result<()> {
        let env = base_environment(&[("A", "x")]);
        assert_eq!(interpolate("$$A $${A} $$_A", &env)?, "$A ${A} $_A");
        assert_eq!(interpolate("$$$A", &env)?, "$$A");
        assert_eq!(interpolate("a$$b", &env)?, "a$b");
        Ok(())
    }

    #[test]
    fn interpolate_reports_the_first_unset_variable() {
        let env = base_environment(&[("A", "x")]);
        assert_eq!(
            interpolate("$A-${MISSING}-$OTHER", &env),
            Err(InterpolationError::Unset {
                variable: "MISSING".to_string()
            })
        );
    }

    #[test]
    fn interpolate_applies_defaults() -> eyre::Result<()> {
        let env = base_environment(&[("SET", "value"), ("EMPTY", ""), ("FALLBACK", "fb")]);

        // `:-` falls back when the variable is unset or empty; `-` only when it is unset.
        assert_eq!(interpolate("${UNSET:-default}", &env)?, "default");
        assert_eq!(interpolate("${EMPTY:-default}", &env)?, "default");
        assert_eq!(interpolate("${SET:-default}", &env)?, "value");
        assert_eq!(interpolate("${UNSET-default}", &env)?, "default");
        assert_eq!(interpolate("${EMPTY-default}", &env)?, "");
        assert_eq!(interpolate("${SET-default}", &env)?, "value");

        // An empty default is the explicit way to ask for an empty substitution.
        assert_eq!(interpolate("[${UNSET:-}]", &env)?, "[]");

        // Defaults may contain references, nested braces, and literal punctuation.
        assert_eq!(interpolate("${UNSET:-${FALLBACK}}", &env)?, "fb");
        assert_eq!(interpolate("${UNSET:-${ALSO:-deep}}", &env)?, "deep");
        assert_eq!(interpolate("${UNSET:-./a-b:c}", &env)?, "./a-b:c");
        Ok(())
    }

    #[test]
    fn interpolate_rejects_malformed_references() {
        let env = base_environment(&[("A", "x")]);
        assert_eq!(
            interpolate("${A", &env),
            Err(InterpolationError::Unterminated)
        );
        assert_eq!(
            interpolate("${A:-${B}", &env),
            Err(InterpolationError::Unterminated)
        );
        for reference in ["", "1A", "A|b", "A:"] {
            let input = format!("${{{reference}}}");
            assert_eq!(
                interpolate(&input, &env),
                Err(InterpolationError::InvalidReference {
                    reference: reference.to_string(),
                }),
                "{input}"
            );
        }
        for (input, operator) in [("${A:?msg}", ":?"), ("${A:+x}", ":+"), ("${A=x}", "=")] {
            assert_eq!(
                interpolate(input, &env),
                Err(InterpolationError::UnsupportedOperator {
                    operator: operator.to_string(),
                }),
                "{input}"
            );
        }
    }

    #[test]
    fn interpolate_bounds_default_nesting() {
        let env = base_environment(&[]);
        let nested = (0..=MAX_DEFAULT_NESTING)
            .fold("x".to_string(), |inner, _| format!("${{UNSET:-{inner}}}"));
        assert_eq!(
            interpolate(&nested, &env),
            Err(InterpolationError::NestingTooDeep)
        );
    }

    #[test]
    fn expand_env_values_is_single_pass_and_ordered() -> eyre::Result<()> {
        let base = base_environment(&[("X", "base")]);

        let mut env = RawEnvMap::new();
        env.insert("A", RawValue::Expandable("${X}-a".to_string()));
        env.insert("B", RawValue::Expandable("${A}-b".to_string()));

        let out = expand_env_values(&env, &base)?;
        assert_eq!(out.inner.get("A").map(String::as_str), Some("base-a"));
        assert_eq!(out.inner.get("B").map(String::as_str), Some("base-a-b"));
        Ok(())
    }

    /// A forward reference is unset at the time its line is expanded, so it is an error rather
    /// than a silent empty substitution.
    #[test]
    fn expand_env_values_rejects_forward_references() {
        let mut env = RawEnvMap::new();
        env.insert("B", RawValue::Expandable("${A}-b".to_string()));
        env.insert("A", RawValue::Expandable("a".to_string()));

        assert_eq!(
            expand_env_values(&env, &HashMap::new()),
            Err(ValueError {
                key: "B".to_string(),
                source: InterpolationError::Unset {
                    variable: "A".to_string()
                },
            })
        );
    }

    #[test]
    fn dotenv_allows_export_with_extra_whitespace() -> eyre::Result<()> {
        let env = parse_dotenv("export   FOO=bar\nexport\tBAZ=qux\n")?;
        assert_eq!(env.inner.get("FOO").map(RawValue::as_str), Some("bar"));
        assert_eq!(env.inner.get("BAZ").map(RawValue::as_str), Some("qux"));
        Ok(())
    }

    #[test]
    fn dotenv_strips_inline_comments_outside_quotes() -> eyre::Result<()> {
        let env = parse_dotenv("FOO=bar # comment\nBAR=\"x # y\" # z\n")?;
        assert_eq!(env.inner.get("FOO").map(RawValue::as_str), Some("bar"));
        assert_eq!(env.inner.get("BAR").map(RawValue::as_str), Some("x # y"));
        Ok(())
    }

    #[test]
    fn dotenv_double_quote_unescapes_common_sequences() -> eyre::Result<()> {
        let env = parse_dotenv("A=\"x\\n\\\"y\\\"\\\\z\"\n")?;
        assert_eq!(
            env.inner.get("A").map(RawValue::as_str),
            Some("x\n\"y\"\\z")
        );
        Ok(())
    }

    #[test]
    fn single_quoted_values_are_literal_after_expansion() -> eyre::Result<()> {
        let env = parse_dotenv("PASS='s3cr$t!'\nLIT='${X}'\nDOLLARS='$$'\n")?;
        let out = expand_env_values(&env, &HashMap::new())?;
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
        let out = expand_env_values(&env, &base)?;
        assert_eq!(
            out.inner.get("GREETING").map(String::as_str),
            Some("hello world")
        );
        Ok(())
    }

    #[test]
    fn dotenv_stray_quote_does_not_disable_comment_stripping() -> eyre::Result<()> {
        let env = parse_dotenv("FOO=don't # comment\nPRICE=5\" # usd\n")?;
        assert_eq!(env.inner.get("FOO").map(RawValue::as_str), Some("don't"));
        assert_eq!(env.inner.get("PRICE").map(RawValue::as_str), Some("5\""));
        Ok(())
    }

    #[test]
    fn resolve_path_substitutes_and_anchors_relative_results() -> eyre::Result<()> {
        let env = base_environment(&[("SUBDIR", "svc")]);
        let config_dir = Path::new("/project");

        assert_eq!(
            resolve_path(config_dir, "./${SUBDIR}/.env", &env)?,
            Path::new("/project/./svc/.env")
        );
        assert_eq!(
            resolve_path(config_dir, "/abs/${SUBDIR}", &env)?,
            Path::new("/abs/svc")
        );
        assert_eq!(
            resolve_path(config_dir, "${UNSET:-fallback}.env", &env)?,
            Path::new("/project/fallback.env")
        );
        Ok(())
    }

    /// An unset variable must never silently widen a path (for example `${ROOT}/x` collapsing
    /// to `/x`).
    #[test]
    fn resolve_path_rejects_unset_variables() {
        assert_eq!(
            resolve_path(Path::new("/project"), "${ROOT}/service", &HashMap::new()),
            Err(InterpolationError::Unset {
                variable: "ROOT".to_string()
            })
        );
    }

    #[test]
    fn env_file_reader_rejects_oversized_files() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(".env");
        std::fs::write(&path, vec![b'x'; MAX_ENV_FILE_BYTES + 1])?;

        let error = load_env_files_sync(std::slice::from_ref(&path))
            .expect_err("oversized env file should be rejected");

        assert!(matches!(error, Error::FileTooLarge { path: actual } if actual == path));
        Ok(())
    }
}
