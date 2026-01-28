pub mod v1;

use crate::diagnostics::{DiagnosticExt, Span, ToDiagnostics};
use crate::service::RestartPolicy;
use codespan_reporting::diagnostic::{Diagnostic, Label};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use yaml_spanned::{Spanned, Value};

pub fn parse<T: serde::de::DeserializeOwned>(
    value: &yaml_spanned::Spanned<Value>,
) -> Result<Spanned<T>, ConfigError> {
    let inner: T = yaml_spanned::from_value(value).map_err(|source| ConfigError::Serde {
        source,
        span: value.span().into(),
    })?;
    Ok(Spanned::new(value.span, inner))
}

pub fn parse_optional<T: serde::de::DeserializeOwned>(
    value: Option<&yaml_spanned::Spanned<Value>>,
) -> Result<Option<Spanned<T>>, ConfigError> {
    value.map(|value| parse(value)).transpose()
}

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

pub fn config_file_names() -> impl Iterator<Item = &'static str> {
    [
        "micromux.yaml",
        ".micromux.yaml",
        "micromux.yml",
        ".micromux.yml",
    ]
    .into_iter()
}

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

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
pub enum Version {
    #[serde(rename = "1", alias = "v1", alias = "V1")]
    V1,
    #[serde(rename = "latest")]
    Latest,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UiConfig {
    pub width: Option<Spanned<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub ui_config: UiConfig,
    pub services: IndexMap<Spanned<String>, Service>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigFile<F> {
    pub file_id: F,
    pub config_dir: PathBuf,
    pub config: Config,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum DependencyCondition {
    #[default]
    #[serde(
        rename = "service_started",
        alias = "service-started",
        alias = "ServiceStarted",
        alias = "started"
    )]
    ServiceStarted,
    #[serde(
        rename = "service_healthy",
        alias = "service-healthy",
        alias = "ServiceHealthy",
        alias = "healthy"
    )]
    ServiceHealthy,
    #[serde(
        rename = "service_completed_successfully",
        alias = "service-completed-successfully",
        alias = "ServiceCompletedSuccessfully",
        alias = "completed"
    )]
    ServiceCompletedSuccessfully,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub name: Spanned<String>,
    pub condition: Option<Spanned<DependencyCondition>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFile {
    pub path: Spanned<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub name: Spanned<String>,
    pub command: (Spanned<String>, Vec<Spanned<String>>),
    pub working_dir: Option<Spanned<String>>,
    pub env_file: Vec<EnvFile>,
    pub environment: IndexMap<Spanned<String>, Spanned<String>>,
    pub depends_on: Vec<Dependency>,
    pub healthcheck: Option<HealthCheck>,
    pub ports: Vec<Spanned<String>>,
    pub restart: Option<RestartPolicy>,
    pub color: Option<Spanned<bool>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCheck {
    /// The healthcheck test.
    ///
    /// E.g. ("pg_isready", ["-U", "postgres"])
    pub test: (Spanned<String>, Vec<Spanned<String>>),
    pub start_delay: Option<Spanned<std::time::Duration>>,
    /// e.g. "30s"
    pub interval: Option<Spanned<std::time::Duration>>,
    /// e.g. "10s"
    pub timeout: Option<Spanned<std::time::Duration>>,
    /// Number of retries before marking unhealthy.
    pub retries: Option<Spanned<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InvalidCommandReason {
    FailedToSplit,
    EmptyCommand,
}

#[derive(thiserror::Error, Debug)]
pub enum ConfigError {
    #[error("invalid command {command}: {reason:?}")]
    InvalidCommand {
        command: String,
        reason: InvalidCommandReason,
        span: Span,
    },
    #[error("invalid duration {duration}")]
    InvalidDuration {
        duration: String,
        span: Span,
        #[source]
        source: humantime::DurationError,
    },
    #[error("{message}")]
    MissingKey {
        key: String,
        message: String,
        span: Span,
    },
    #[error("{message}")]
    UnexpectedType {
        message: String,
        expected: Vec<yaml_spanned::value::Kind>,
        found: yaml_spanned::value::Kind,
        span: Span,
    },
    #[error("{message}")]
    InvalidValue { message: String, span: Span },
    #[error("{source}")]
    Serde {
        #[source]
        source: yaml_spanned::error::SerdeError,
        span: Span,
    },
    #[error(transparent)]
    YAML(#[from] yaml_spanned::Error),
}

impl ToDiagnostics for ConfigError {
    fn to_diagnostics<F: Copy + PartialEq>(&self, file_id: F) -> Vec<Diagnostic<F>> {
        match self {
            Self::InvalidCommand {
                command,
                span,
                reason,
            } => {
                let mut labels = vec![];
                match reason {
                    InvalidCommandReason::FailedToSplit => {
                        labels.push(
                            Label::secondary(file_id, span.clone())
                                .with_message("failed to split command"),
                        );
                    }
                    InvalidCommandReason::EmptyCommand => {
                        labels.push(
                            Label::secondary(file_id, span.clone()).with_message("empty command"),
                        );
                    }
                }
                let mut diagnostics = vec![
                    Diagnostic::error()
                        .with_message(format!("invalid command `{command}`"))
                        .with_labels(labels),
                ];
                match reason {
                    InvalidCommandReason::FailedToSplit => {
                        diagnostics.push(
                            Diagnostic::help().with_message("try using a sequence".to_string()),
                        );
                    }
                    InvalidCommandReason::EmptyCommand => {
                        diagnostics.push(
                            Diagnostic::help().with_message("use a non-empty command".to_string()),
                        );
                    }
                }
                diagnostics
            }
            Self::InvalidDuration { duration, span, .. } => vec![
                Diagnostic::error()
                    .with_message(format!("invalid duration `{duration}`"))
                    .with_labels(vec![
                        Label::secondary(file_id, span.clone())
                            .with_message("cannot parse as duration"),
                    ]),
                Diagnostic::help().with_message("Duration must have a valid format like `2min 2s`"),
            ],
            Self::MissingKey {
                message, key, span, ..
            } => vec![
                Diagnostic::error()
                    .with_message(format!("missing required key `{key}`"))
                    .with_labels(vec![
                        Label::secondary(file_id, span.clone()).with_message(message),
                    ]),
            ],
            Self::UnexpectedType {
                expected,
                found,
                span,
                ..
            } => {
                let expected = expected
                    .iter()
                    .map(|ty| format!("`{ty:?}`"))
                    .collect::<Vec<_>>()
                    .join(", or ");
                let diagnostic = Diagnostic::error()
                    .with_message(self.to_string())
                    .with_labels(vec![
                        Label::primary(file_id, span.clone())
                            .with_message(format!("expected {expected}")),
                    ])
                    .with_notes(vec![unindent::unindent(&format!(
                        "
                        expected type {expected}
                           found type `{found:?}`
                        "
                    ))]);
                vec![diagnostic]
            }
            Self::InvalidValue { message, span } => vec![
                Diagnostic::error()
                    .with_message(message.to_string())
                    .with_labels(vec![
                        Label::primary(file_id, span.clone()).with_message(message.to_string()),
                    ]),
            ],
            Self::Serde { source, span } => vec![
                Diagnostic::error()
                    .with_message(self.to_string())
                    .with_labels(vec![
                        Label::primary(file_id, span.clone()).with_message(source.to_string()),
                    ]),
            ],
            Self::YAML(source) => {
                use yaml_spanned::error::ToDiagnostics;
                source.to_diagnostics(file_id)
            }
        }
    }
}

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
    use color_eyre::eyre;
    use indoc::indoc;

    #[test]
    fn parse_config() -> eyre::Result<()> {
        let _yaml = indoc! {r#"
            version: "3"
            services:
              app:
                command: "./start.sh"
                environment:
                  APP_ENV: production
                  APP_DEBUG: "false"
                depends_on:
                  - db
                healthcheck:
                  test: ["CMD-SHELL", "curl -f http://localhost/health || exit 1"]
                  interval: "30s"
                  timeout: "10s"
                  retries: 3
              db:
                environment:
                  POSTGRES_PASSWORD: example
                healthcheck:
                  test: ["CMD", "pg_isready", "-U", "postgres"]
                  interval: "10s"
                  timeout: "5s"
                  retries: 5
        "#};

        // TODO: complete this test
        // let config = super::from_str(yaml)?;
        // println!("{:#?}", config);
        Ok(())
    }
}
