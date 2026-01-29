//! Configuration parsing and types.
//!
//! This module provides:
//! - [`from_str`]: parse a YAML configuration into a typed [`ConfigFile`].
//! - [`find_config_file`]: locate a config file in a directory.
//! - A set of configuration types (e.g. [`Service`], [`HealthCheck`]) and diagnostics-friendly
//!   errors ([`ConfigError`]).

pub mod v1;

use crate::diagnostics::{DiagnosticExt, Span, ToDiagnostics};
use crate::service::RestartPolicy;
use codespan_reporting::diagnostic::{Diagnostic, Label};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use yaml_spanned::{Spanned, Value};

/// Parse a value into a typed, spanned value.
///
/// # Errors
///
/// Returns [`ConfigError::Serde`] if `value` cannot be deserialized into `T`.
pub fn parse<T: serde::de::DeserializeOwned>(
    value: &yaml_spanned::Spanned<Value>,
) -> Result<Spanned<T>, ConfigError> {
    let inner: T = yaml_spanned::from_value(value).map_err(|source| ConfigError::Serde {
        source,
        span: value.span().into(),
    })?;
    Ok(Spanned::new(value.span, inner))
}

/// Parse an optional value into a typed, spanned value.
///
/// # Errors
///
/// Returns the same errors as [`parse`] if the value is present but cannot be parsed.
pub fn parse_optional<T: serde::de::DeserializeOwned>(
    value: Option<&yaml_spanned::Spanned<Value>>,
) -> Result<Option<Spanned<T>>, ConfigError> {
    value.map(|value| parse(value)).transpose()
}

/// Parse an optional duration.
///
/// The duration must be provided as a string compatible with [`humantime::parse_duration`]
/// (e.g. `"30s"`, `"2min"`).
///
/// # Errors
///
/// Returns an error if the value is not a string ([`ConfigError::UnexpectedType`]) or if the
/// duration cannot be parsed ([`ConfigError::InvalidDuration`]).
pub fn parse_duration(
    value: Option<&yaml_spanned::Spanned<Value>>,
) -> Result<Option<Spanned<std::time::Duration>>, ConfigError> {
    value
        .map(|value| {
            let raw_duration = value.as_str().ok_or_else(|| ConfigError::UnexpectedType {
                message: "duration must be a string".to_string(),
                found: value.kind(),
                expected: vec![yaml_spanned::value::Kind::String],
                span: value.span().into(),
            })?;
            let duration = humantime::parse_duration(raw_duration).map_err(|source| {
                ConfigError::InvalidDuration {
                    duration: value.to_string(),
                    span: value.span().into(),
                    source,
                }
            })?;
            Ok(Spanned {
                inner: duration,
                span: *value.span(),
            })
        })
        .transpose()
}

/// Candidate configuration file names, in lookup order.
pub fn config_file_names() -> impl Iterator<Item = &'static str> {
    [
        "micromux.yaml",
        ".micromux.yaml",
        "micromux.yml",
        ".micromux.yml",
    ]
    .into_iter()
}

/// Find a configuration file in the given directory.
///
/// The search checks [`config_file_names`] concurrently and returns the first file that exists.
///
/// # Errors
///
/// Returns an error if a file exists but cannot be canonicalized for reasons other than
/// `NotFound`.
pub async fn find_config_file(dir: &Path) -> std::io::Result<Option<PathBuf>> {
    use futures::{StreamExt, TryStreamExt};
    let mut found = futures::stream::iter(config_file_names().map(|path| dir.join(path)))
        .map(|path| async move {
            match tokio::fs::canonicalize(&path).await {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(err),
                Ok(path) => Ok(Some(path)),
            }
        })
        .buffered(8)
        .into_stream();

    while let Some(path) = found.try_next().await? {
        if let Some(path) = path {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Configuration file format version.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
pub enum Version {
    /// Version 1 of the configuration format.
    #[serde(rename = "1", alias = "v1", alias = "V1")]
    V1,
    /// Alias for the latest supported version.
    #[serde(rename = "latest")]
    Latest,
}

/// User-interface related configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UiConfig {
    /// Optional desired UI width.
    pub width: Option<Spanned<usize>>,
}

/// Parsed configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Configuration for the UI.
    pub ui_config: UiConfig,
    /// Service definitions keyed by service name.
    pub services: IndexMap<Spanned<String>, Service>,
}

/// A parsed config file together with its origin metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile<F> {
    /// File identifier used in diagnostics.
    pub file_id: F,
    /// Directory the config was loaded from.
    pub config_dir: PathBuf,
    /// Parsed config contents.
    pub config: Config,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
/// Condition that must be satisfied before a dependent service is considered ready.
pub enum DependencyCondition {
    /// Dependency must have started.
    #[default]
    #[serde(
        rename = "service_started",
        alias = "service-started",
        alias = "ServiceStarted",
        alias = "started"
    )]
    Started,
    /// Dependency must be healthy.
    #[serde(
        rename = "service_healthy",
        alias = "service-healthy",
        alias = "ServiceHealthy",
        alias = "healthy"
    )]
    Healthy,
    /// Dependency must have completed successfully.
    #[serde(
        rename = "service_completed_successfully",
        alias = "service-completed-successfully",
        alias = "ServiceCompletedSuccessfully",
        alias = "completed"
    )]
    CompletedSuccessfully,
}

/// A dependency on another service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// Name of the dependent service.
    pub name: Spanned<String>,
    /// Optional condition that must be met.
    pub condition: Option<Spanned<DependencyCondition>>,
}

/// A `.env` file reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFile {
    /// Path to the `.env` file (relative to `config_dir` unless absolute).
    pub path: Spanned<String>,
}

/// Service configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// Service name.
    pub name: Spanned<String>,
    /// Command to execute and its arguments.
    pub command: (Spanned<String>, Vec<Spanned<String>>),
    /// Optional working directory.
    pub working_dir: Option<Spanned<String>>,
    /// Environment files to load.
    pub env_file: Vec<EnvFile>,
    /// Explicit environment variables.
    pub environment: IndexMap<Spanned<String>, Spanned<String>>,
    /// Dependencies on other services.
    pub depends_on: Vec<Dependency>,
    /// Optional healthcheck configuration.
    pub healthcheck: Option<HealthCheck>,
    /// Port mappings / port specs.
    pub ports: Vec<Spanned<String>>,
    /// Restart policy for this service.
    pub restart: Option<RestartPolicy>,
    /// Whether this service should be rendered in color.
    pub color: Option<Spanned<bool>>,
}

/// Healthcheck configuration for a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCheck {
    /// The healthcheck test.
    ///
    /// For example, `( "pg_isready", ["-U", "postgres"] )`.
    pub test: (Spanned<String>, Vec<Spanned<String>>),
    /// Optional delay before the first healthcheck.
    pub start_delay: Option<Spanned<std::time::Duration>>,
    /// Healthcheck interval (e.g. `"30s"`).
    pub interval: Option<Spanned<std::time::Duration>>,
    /// Healthcheck timeout (e.g. `"10s"`).
    pub timeout: Option<Spanned<std::time::Duration>>,
    /// Number of retries before marking unhealthy.
    pub retries: Option<Spanned<usize>>,
}

/// Reason why a command is invalid.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InvalidCommandReason {
    /// The command string could not be split into an argv-like representation.
    FailedToSplit,
    /// The command is empty.
    EmptyCommand,
}

/// Errors that can occur while parsing configuration.
#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    #[error("invalid command {command}: {reason:?}")]
    /// A command could not be parsed.
    InvalidCommand {
        /// Raw command string.
        command: String,
        /// Why the command is invalid.
        reason: InvalidCommandReason,
        /// Span of the command value.
        span: Span,
    },
    #[error("invalid duration {duration}")]
    /// A duration value could not be parsed.
    InvalidDuration {
        /// Raw duration string.
        duration: String,
        /// Span of the duration value.
        span: Span,
        #[source]
        /// Underlying parse error.
        source: humantime::DurationError,
    },
    #[error("{message}")]
    /// A required mapping key was missing.
    MissingKey {
        /// Missing key.
        key: String,
        /// User-facing error message.
        message: String,
        /// Span of the mapping where the key is missing.
        span: Span,
    },
    #[error("{message}")]
    /// A YAML value had an unexpected type.
    UnexpectedType {
        /// User-facing error message.
        message: String,
        /// Expected YAML kinds.
        expected: Vec<yaml_spanned::value::Kind>,
        /// Actual YAML kind.
        found: yaml_spanned::value::Kind,
        /// Span of the offending value.
        span: Span,
    },
    #[error("{message}")]
    /// A YAML value was syntactically valid but semantically invalid.
    InvalidValue {
        /// User-facing error message.
        message: String,
        /// Span of the offending value.
        span: Span,
    },
    #[error("{source}")]
    /// A serde-based deserialization error occurred.
    Serde {
        #[source]
        /// Underlying serde error.
        source: yaml_spanned::error::SerdeError,
        /// Span of the offending value.
        span: Span,
    },
    #[error(transparent)]
    /// A YAML parser error occurred.
    YAML(#[from] yaml_spanned::Error),
}

impl ToDiagnostics for ConfigError {
    fn to_diagnostics<F: Copy + PartialEq>(&self, file_id: F) -> Vec<Diagnostic<F>> {
        match self {
            Self::InvalidCommand {
                command,
                span,
                reason,
            } => Self::invalid_command_diagnostics(file_id, command, span, reason),
            Self::InvalidDuration { duration, span, .. } => {
                Self::invalid_duration_diagnostics(file_id, duration, span)
            }
            Self::MissingKey {
                message, key, span, ..
            } => Self::missing_key_diagnostics(file_id, key, message, span),
            Self::UnexpectedType {
                expected,
                found,
                span,
                ..
            } => Self::unexpected_type_diagnostics(file_id, self, expected, *found, span),
            Self::InvalidValue { message, span } => {
                Self::invalid_value_diagnostics(file_id, message, span)
            }
            Self::Serde { source, span } => Self::serde_diagnostics(file_id, self, source, span),
            Self::YAML(source) => {
                use yaml_spanned::error::ToDiagnostics;
                source.to_diagnostics(file_id)
            }
        }
    }
}

impl ConfigError {
    fn invalid_command_diagnostics<F: Copy + PartialEq>(
        file_id: F,
        command: &str,
        span: &Span,
        reason: &InvalidCommandReason,
    ) -> Vec<Diagnostic<F>> {
        let mut labels = vec![];
        match reason {
            InvalidCommandReason::FailedToSplit => {
                labels.push(
                    Label::secondary(file_id, span.clone()).with_message("failed to split command"),
                );
            }
            InvalidCommandReason::EmptyCommand => {
                labels.push(Label::secondary(file_id, span.clone()).with_message("empty command"));
            }
        }

        let mut diagnostics = vec![
            Diagnostic::error()
                .with_message(format!("invalid command `{command}`"))
                .with_labels(labels),
        ];

        match reason {
            InvalidCommandReason::FailedToSplit => {
                diagnostics
                    .push(Diagnostic::help().with_message("try using a sequence".to_string()));
            }
            InvalidCommandReason::EmptyCommand => {
                diagnostics
                    .push(Diagnostic::help().with_message("use a non-empty command".to_string()));
            }
        }

        diagnostics
    }

    fn invalid_duration_diagnostics<F: Copy + PartialEq>(
        file_id: F,
        duration: &str,
        span: &Span,
    ) -> Vec<Diagnostic<F>> {
        vec![
            Diagnostic::error()
                .with_message(format!("invalid duration `{duration}`"))
                .with_labels(vec![
                    Label::secondary(file_id, span.clone())
                        .with_message("cannot parse as duration"),
                ]),
            Diagnostic::help().with_message("Duration must have a valid format like `2min 2s`"),
        ]
    }

    fn missing_key_diagnostics<F: Copy + PartialEq>(
        file_id: F,
        key: &str,
        message: &str,
        span: &Span,
    ) -> Vec<Diagnostic<F>> {
        vec![
            Diagnostic::error()
                .with_message(format!("missing required key `{key}`"))
                .with_labels(vec![
                    Label::secondary(file_id, span.clone()).with_message(message),
                ]),
        ]
    }

    fn unexpected_type_diagnostics<F: Copy + PartialEq>(
        file_id: F,
        this: &Self,
        expected: &[yaml_spanned::value::Kind],
        found: yaml_spanned::value::Kind,
        span: &Span,
    ) -> Vec<Diagnostic<F>> {
        let expected = expected
            .iter()
            .map(|ty| format!("`{ty:?}`"))
            .collect::<Vec<_>>()
            .join(", or ");
        vec![
            Diagnostic::error()
                .with_message(this.to_string())
                .with_labels(vec![
                    Label::primary(file_id, span.clone())
                        .with_message(format!("expected {expected}")),
                ])
                .with_notes(vec![unindent::unindent(&format!(
                    "
                    expected type {expected}
                       found type `{found:?}`
                    "
                ))]),
        ]
    }

    fn invalid_value_diagnostics<F: Copy + PartialEq>(
        file_id: F,
        message: &str,
        span: &Span,
    ) -> Vec<Diagnostic<F>> {
        vec![
            Diagnostic::error()
                .with_message(message.to_string())
                .with_labels(vec![
                    Label::primary(file_id, span.clone()).with_message(message.to_string()),
                ]),
        ]
    }

    fn serde_diagnostics<F: Copy + PartialEq>(
        file_id: F,
        this: &Self,
        source: &yaml_spanned::error::SerdeError,
        span: &Span,
    ) -> Vec<Diagnostic<F>> {
        vec![
            Diagnostic::error()
                .with_message(this.to_string())
                .with_labels(vec![
                    Label::primary(file_id, span.clone()).with_message(source.to_string()),
                ]),
        ]
    }
}

/// Parse the `version` field of the config.
///
/// # Errors
///
/// Returns an error if the version field exists but cannot be parsed.
pub fn parse_version<F>(
    value: &yaml_spanned::Spanned<Value>,
    file_id: F,
    strict: Option<bool>,
    diagnostics: &mut Vec<Diagnostic<F>>,
) -> Result<Version, ConfigError> {
    match value.get("version") {
        None => {
            let diagnostic = Diagnostic::warning_or_error(strict.unwrap_or(false))
                .with_message("missing version")
                .with_labels(vec![
                    Label::primary(file_id, value.span)
                        .with_message("no version is specified - assuming version 1"),
                ]);
            diagnostics.push(diagnostic);
            Ok(Version::Latest)
        }
        Some(yaml_spanned::Spanned {
            inner: Value::Number(n),
            ..
        }) if n.as_f64() == Some(1.0) => Ok(Version::V1),
        Some(value) => {
            let version = parse::<Version>(value)?;
            Ok(version.into_inner())
        }
    }
}

/// Parse a micromux configuration from a YAML string.
///
/// # Errors
///
/// Returns an error if the YAML cannot be parsed or if the resulting value does not match the
/// expected schema.
pub fn from_str<F: Copy + PartialEq>(
    raw_config: &str,
    config_dir: &Path,
    file_id: F,
    strict: Option<bool>,
    diagnostics: &mut Vec<Diagnostic<F>>,
) -> Result<ConfigFile<F>, ConfigError> {
    let value = yaml_spanned::from_str(raw_config).map_err(ConfigError::YAML)?;
    let version = parse_version(&value, file_id, strict, diagnostics)?;
    let config = match version {
        Version::Latest | Version::V1 => v1::parse_config(&value, file_id, strict, diagnostics)?,
    };

    Ok(ConfigFile {
        file_id,
        config_dir: config_dir.to_path_buf(),
        config,
    })
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use jsonschema::{Draft, Validator};
    use std::path::Path;

    fn compiled_schema() -> color_eyre::eyre::Result<Validator> {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../../micromux.schema.json"))?;
        let schema: &'static serde_json::Value = Box::leak(Box::new(schema));
        Ok(jsonschema::options()
            .with_draft(Draft::Draft7)
            .build(schema)?)
    }

    #[test]
    fn parse_config_basic_and_special_cases() -> color_eyre::eyre::Result<()> {
        let yaml = indoc! {r#"
            version: "1"
            ui:
              width: 80
            services:
              app:
                # string form command
                command: "./start.sh --flag"
                # env_file can be a string
                env_file: ".env"
                environment:
                  APP_ENV: production
                  APP_DEBUG: "false"
                ports: [8080]
                restart: on-failure=3
                depends_on:
                  - db
                healthcheck:
                  test: ["CMD-SHELL", "curl -f http://localhost/health || exit 1"]
                  interval: "30s"
                  timeout: "10s"
                  retries: 3
              db:
                # array form command
                command: ["CMD", "postgres", "-c", "fsync=off"]
                env_file:
                  - path: "./db.env"
                depends_on:
                  - name: app
                    condition: healthy
                restart: unless-stopped
                healthcheck:
                  test: ["CMD", "pg_isready", "-U", "postgres"]
                  interval: "10s"
                  timeout: "5s"
                  retries: 5
        "#};

        let mut diagnostics = vec![];
        let parsed = super::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)?;
        assert!(diagnostics.is_empty());

        // UI config
        assert_eq!(parsed.config.ui_config.width.as_deref().copied(), Some(80));

        // Find services without indexing (clippy::indexing_slicing is denied)
        let app = parsed
            .config
            .services
            .iter()
            .find(|(name, _svc)| name.as_ref() == "app")
            .map(|(_name, svc)| svc)
            .ok_or_else(|| color_eyre::eyre::eyre!("missing service 'app'"))?;

        let db = parsed
            .config
            .services
            .iter()
            .find(|(name, _svc)| name.as_ref() == "db")
            .map(|(_name, svc)| svc)
            .ok_or_else(|| color_eyre::eyre::eyre!("missing service 'db'"))?;

        // command parsing: string form is split
        assert_eq!(app.command.0.as_ref(), "./start.sh");
        assert!(app.command.1.iter().any(|v| v.as_ref() == "--flag"));

        // env_file parsing: string + mapping forms
        assert_eq!(app.env_file.len(), 1);
        let app_env_file = app
            .env_file
            .first()
            .ok_or_else(|| color_eyre::eyre::eyre!("missing app.env_file entry"))?;
        assert_eq!(app_env_file.path.as_ref(), ".env");
        assert_eq!(db.env_file.len(), 1);
        let db_env_file = db
            .env_file
            .first()
            .ok_or_else(|| color_eyre::eyre::eyre!("missing db.env_file entry"))?;
        assert_eq!(db_env_file.path.as_ref(), "./db.env");

        // depends_on parsing: string + mapping condition alias
        assert_eq!(app.depends_on.len(), 1);
        let app_dep = app
            .depends_on
            .first()
            .ok_or_else(|| color_eyre::eyre::eyre!("missing app.depends_on entry"))?;
        assert_eq!(app_dep.name.as_ref(), "db");
        assert_eq!(db.depends_on.len(), 1);
        let db_dep = db
            .depends_on
            .first()
            .ok_or_else(|| color_eyre::eyre::eyre!("missing db.depends_on entry"))?;
        assert_eq!(db_dep.name.as_ref(), "app");
        assert_eq!(
            db_dep.condition.as_ref().map(|c| *c.as_ref()),
            Some(super::DependencyCondition::Healthy)
        );

        // restart parsing
        match &app.restart {
            Some(crate::service::RestartPolicy::OnFailure { remaining_attempts }) => {
                assert_eq!(*remaining_attempts, 3);
            }
            other => {
                return Err(color_eyre::eyre::eyre!(format!(
                    "unexpected restart policy for app: {other:?}"
                )));
            }
        }
        assert!(matches!(
            &db.restart,
            Some(crate::service::RestartPolicy::UnlessStopped)
        ));

        Ok(())
    }

    #[test]
    fn parse_config_missing_version_emits_warning_and_defaults_to_v1()
    -> color_eyre::eyre::Result<()> {
        let yaml = indoc! {r#"
            services:
              app:
                command: "echo hello"
        "#};

        let mut diagnostics = vec![];
        let parsed = super::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)?;

        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("missing version"))
        );
        assert!(
            parsed
                .config
                .services
                .iter()
                .any(|(name, _svc)| name.as_ref() == "app")
        );
        Ok(())
    }

    #[test]
    fn parse_config_errors_on_missing_command() {
        let yaml = indoc! {r#"
            version: "1"
            services:
              app:
                environment:
                  APP_ENV: production
        "#};

        let mut diagnostics = vec![];
        let result = super::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics);
        assert!(result.is_err());
    }

    #[test]
    fn schema_validates_complex_config() -> color_eyre::eyre::Result<()> {
        let compiled = compiled_schema()?;

        let yaml = indoc! {r#"
            version: 1
            ui:
              width: 120
            services:
              api:
                command: ["CMD", "sh", "-c", "echo api"]
                cwd: ./services/api
                env_file:
                  - "./.env"
                  - path: "./.env.local"
                environment:
                  APP_ENV: production
                  FEATURE_FLAG: true
                  TIMEOUT_MS: 1500
                depends_on:
                  - db
                  - name: cache
                    condition: service_healthy
                ports:
                  - "8080"
                  - 9090
                restart: on-failure:5
                color: false
                healthcheck:
                  test: "CMD-SHELL curl -f http://localhost:8080/health || exit 1"
                  interval: "30s"
                  timeout: "10s"
                  retries: 3
                  initial_delay: "2s"
              db:
                command: "postgres -c fsync=off"
                working_dir: ./services/db
                environment:
                  POSTGRES_PASSWORD: example
                depends_on: []
                ports: []
                restart: unless-stopped
                healthcheck:
                  test: ["CMD", "pg_isready", "-U", "postgres"]
                  interval: "10s"
                  timeout: "5s"
                  retries: 5
        "#};

        let instance: serde_json::Value = serde_yaml::from_str(yaml)?;
        if let Err(err) = compiled.validate(&instance) {
            let message = std::iter::once(err)
                .chain(compiled.iter_errors(&instance))
                .map(|err| err.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            return Err(color_eyre::eyre::eyre!(message));
        }

        Ok(())
    }

    #[test]
    fn schema_rejects_missing_command() -> color_eyre::eyre::Result<()> {
        let compiled = compiled_schema()?;

        let yaml = indoc! {r#"
            version: "1"
            services:
              api:
                environment:
                  APP_ENV: production
        "#};
        let instance: serde_json::Value = serde_yaml::from_str(yaml)?;
        assert!(compiled.validate(&instance).is_err());
        Ok(())
    }
}
