use super::{
    Config, ConfigError, HealthCheckDefaults, Service, UiConfig, parse, parse_duration,
    parse_optional,
};
use crate::diagnostics::DiagnosticExt;
use crate::{
    DiskLogRetention, LogLimit, LogRetention, config::InvalidCommandReason, service::RestartPolicy,
};
use codespan_reporting::diagnostic::{Diagnostic, Label};
use indexmap::IndexMap;
use itertools::Itertools;
use yaml_spanned::{Mapping, Sequence, Spanned, Value, value::Kind};

/// Known top-level keys for a service definition (including accepted aliases). Used to warn
/// about typos that would otherwise silently drop an entire config block.
const KNOWN_SERVICE_KEYS: &[&str] = &[
    "name",
    "command",
    "working_dir",
    "cwd",
    "directory",
    "env_file",
    "environment",
    "depends_on",
    "healthcheck",
    "ports",
    "restart",
    "color",
    "logs",
];

const KNOWN_HEALTHCHECK_TIMING_KEYS: &[&str] = &[
    "start_delay",
    "startup_delay",
    "initial_delay",
    "interval",
    "timeout",
    "retries",
];

const KNOWN_HEALTHCHECK_KEYS: &[&str] = &[
    "test",
    "start_delay",
    "startup_delay",
    "initial_delay",
    "interval",
    "timeout",
    "retries",
];

/// Warn (or, in strict mode, error) about mapping keys that the parser does not recognize.
fn warn_unknown_keys<F: Copy>(
    mapping: &Mapping,
    known: &[&str],
    context: &str,
    file_id: F,
    strict: bool,
    diagnostics: &mut Vec<Diagnostic<F>>,
) {
    for (key, _value) in mapping {
        let Some(key_name) = key.as_str() else {
            continue;
        };
        if !known.contains(&key_name) {
            diagnostics.push(
                Diagnostic::warning_or_error(strict)
                    .with_message(format!("unknown {context} field `{key_name}`"))
                    .with_labels(vec![
                        Label::primary(file_id, key.span).with_message("unknown field"),
                    ]),
            );
        }
    }
}

fn parse_string_value(
    value: &yaml_spanned::Spanned<Value>,
    message: &str,
) -> Result<String, ConfigError> {
    match &value.inner {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        other => Err(ConfigError::UnexpectedType {
            message: message.to_string(),
            expected: vec![Kind::String, Kind::Number, Kind::Bool],
            found: other.kind(),
            span: value.span().into(),
        }),
    }
}

fn parse_environment(
    mapping: &yaml_spanned::Mapping,
) -> Result<IndexMap<Spanned<String>, Spanned<String>>, ConfigError> {
    let Some(value) = mapping.get("environment") else {
        return Ok(IndexMap::new());
    };
    let (_span, env_mapping) = expect_mapping(value, "environment must be a mapping".into())?;

    let mut env = IndexMap::new();
    for (k, v) in env_mapping {
        let key = parse::<String>(k)?;
        let raw = parse_string_value(v, "environment values must be scalar")?;
        env.insert(
            key,
            Spanned {
                span: v.span,
                inner: raw,
            },
        );
    }
    Ok(env)
}

fn parse_env_file(mapping: &yaml_spanned::Mapping) -> Result<Vec<super::EnvFile>, ConfigError> {
    let Some(value) = mapping.get("env_file") else {
        return Ok(vec![]);
    };
    let mut env_files = vec![];

    let mut push_item = |item: &Spanned<Value>| -> Result<(), ConfigError> {
        match item {
            Spanned {
                span,
                inner: Value::String(path),
            } => {
                env_files.push(super::EnvFile {
                    path: Spanned {
                        span: *span,
                        inner: path.clone(),
                    },
                });
            }
            Spanned {
                span: _,
                inner: Value::Mapping(m),
            } => {
                let Some(path_value) = m.get("path") else {
                    return Err(ConfigError::MissingKey {
                        key: "path".to_string(),
                        message: "env_file entries must have a 'path'".to_string(),
                        span: item.span().into(),
                    });
                };
                let (path_span, path) =
                    expect_string(path_value, "env_file.path must be a string".into())?;
                env_files.push(super::EnvFile {
                    path: Spanned {
                        span: *path_span,
                        inner: path.clone(),
                    },
                });
            }
            _ => {
                return Err(ConfigError::UnexpectedType {
                    message: "env_file entries must be a string or mapping".to_string(),
                    expected: vec![Kind::String, Kind::Mapping],
                    found: item.kind(),
                    span: item.span().into(),
                });
            }
        }
        Ok(())
    };

    match &value.inner {
        Value::Sequence(seq) => {
            for item in seq {
                push_item(item)?;
            }
        }
        Value::String(_) | Value::Mapping(_) => {
            push_item(value)?;
        }
        other => {
            return Err(ConfigError::UnexpectedType {
                message: "env_file must be a sequence, string, or mapping".to_string(),
                expected: vec![Kind::Sequence, Kind::String, Kind::Mapping],
                found: other.kind(),
                span: value.span().into(),
            });
        }
    }

    Ok(env_files)
}

fn parse_depends_on(
    mapping: &yaml_spanned::Mapping,
) -> Result<Vec<super::Dependency>, ConfigError> {
    let Some(value) = mapping.get("depends_on") else {
        return Ok(vec![]);
    };
    let seq = expect_sequence(value, "depends_on must be a sequence".into())?;
    let mut deps = vec![];
    for item in seq {
        match item {
            Spanned {
                span,
                inner: Value::String(name),
            } => {
                deps.push(super::Dependency {
                    name: Spanned {
                        span: *span,
                        inner: name.clone(),
                    },
                    condition: None,
                });
            }
            Spanned {
                span: _,
                inner: Value::Mapping(m),
            } => {
                let Some(name_value) = m.get("name") else {
                    return Err(ConfigError::MissingKey {
                        key: "name".to_string(),
                        message: "depends_on entries must have a 'name'".to_string(),
                        span: item.span().into(),
                    });
                };
                let name = parse::<String>(name_value)?;
                let condition = parse_optional::<super::DependencyCondition>(m.get("condition"))?;
                deps.push(super::Dependency { name, condition });
            }
            _ => {
                return Err(ConfigError::UnexpectedType {
                    message: "depends_on entries must be a string or mapping".to_string(),
                    expected: vec![Kind::String, Kind::Mapping],
                    found: item.kind(),
                    span: item.span().into(),
                });
            }
        }
    }
    Ok(deps)
}

fn parse_ports(mapping: &yaml_spanned::Mapping) -> Result<Vec<Spanned<String>>, ConfigError> {
    let Some(value) = mapping.get("ports") else {
        return Ok(vec![]);
    };
    let seq = expect_sequence(value, "ports must be a sequence".into())?;
    let mut ports = vec![];
    for item in seq {
        let raw = parse_string_value(item, "ports entries must be a scalar")?;
        ports.push(Spanned {
            span: item.span,
            inner: raw,
        });
    }
    Ok(ports)
}

fn parse_restart_value(value: &yaml_spanned::Spanned<Value>) -> Result<RestartPolicy, ConfigError> {
    let raw = parse_string_value(value, "restart must be a string")?;
    let normalized = raw.trim().to_ascii_lowercase();
    let policy = match normalized.as_str() {
        "always" => RestartPolicy::Always,
        "unless-stopped" | "unless_stopped" => RestartPolicy::UnlessStopped,
        "never" | "no" => RestartPolicy::Never,
        _ => {
            let prefix = "on-failure";
            if let Some(rest) = normalized
                .strip_prefix(prefix)
                .or_else(|| normalized.strip_prefix("on_failure"))
            {
                let rest = rest.trim_start_matches([':', '=']).trim();
                // Bare `on-failure` means restart indefinitely on non-zero exit (Compose
                // semantics); `on-failure:N` caps the number of automatic restarts at N.
                let max_attempts = if rest.is_empty() {
                    None
                } else {
                    Some(
                        rest.parse::<usize>()
                            .map_err(|_| ConfigError::InvalidValue {
                                message: format!("invalid restart policy `{raw}`"),
                                span: value.span().into(),
                            })?,
                    )
                };
                RestartPolicy::OnFailure { max_attempts }
            } else {
                return Err(ConfigError::InvalidValue {
                    message: format!("invalid restart policy `{raw}`"),
                    span: value.span().into(),
                });
            }
        }
    };
    Ok(policy)
}

fn parse_restart(mapping: &yaml_spanned::Mapping) -> Result<Option<RestartPolicy>, ConfigError> {
    mapping.get("restart").map(parse_restart_value).transpose()
}

pub fn expect_sequence<'a>(
    value: &'a yaml_spanned::Spanned<Value>,
    message: Option<&'a str>,
) -> Result<&'a Sequence, ConfigError> {
    value
        .as_sequence()
        .ok_or_else(|| ConfigError::UnexpectedType {
            message: message.unwrap_or("expected sequence").to_string(),
            expected: vec![Kind::Sequence],
            found: value.kind(),
            span: value.span().into(),
        })
}

pub fn expect_mapping<'a>(
    value: &'a yaml_spanned::Spanned<Value>,
    message: Option<&'a str>,
) -> Result<(&'a yaml_spanned::spanned::Span, &'a Mapping), ConfigError> {
    let mapping = value
        .as_mapping()
        .ok_or_else(|| ConfigError::UnexpectedType {
            message: message.unwrap_or("expected mapping").to_string(),
            expected: vec![Kind::Mapping],
            found: value.kind(),
            span: value.span().into(),
        })?;
    Ok((value.span(), mapping))
}

pub fn expect_string<'a>(
    value: &'a yaml_spanned::Spanned<Value>,
    message: Option<&'a str>,
) -> Result<(&'a yaml_spanned::spanned::Span, &'a String), ConfigError> {
    let string = value
        .as_string()
        .ok_or_else(|| ConfigError::UnexpectedType {
            message: message.unwrap_or("expected string").to_string(),
            expected: vec![Kind::String],
            found: value.kind(),
            span: value.span().into(),
        })?;
    Ok((value.span(), string))
}

pub fn parse_ui_config<F: Copy>(
    value: &yaml_spanned::Spanned<Value>,
    file_id: F,
    strict: bool,
    diagnostics: &mut Vec<Diagnostic<F>>,
) -> Result<UiConfig, ConfigError> {
    let Some(value) = value.get("ui") else {
        return Ok(UiConfig::default());
    };
    let (_span, mapping) = expect_mapping(value, "ui config must be a mapping".into())?;
    warn_unknown_keys(
        mapping,
        &["width", "pretty_json_logs"],
        "ui",
        file_id,
        strict,
        diagnostics,
    );
    let width = parse_optional::<usize>(mapping.get("width"))?;
    let pretty_json_logs = parse_optional::<bool>(mapping.get("pretty_json_logs"))?
        .map(Spanned::into_inner)
        .unwrap_or(true);
    Ok(UiConfig {
        width,
        pretty_json_logs,
    })
}

/// Parse the optional top-level `control: { enabled: <bool> }` section. Defaults to enabled.
pub fn parse_control_enabled<F: Copy>(
    value: &yaml_spanned::Spanned<Value>,
    file_id: F,
    strict: bool,
    diagnostics: &mut Vec<Diagnostic<F>>,
) -> Result<bool, ConfigError> {
    let Some(value) = value.get("control") else {
        return Ok(true);
    };
    let (_span, mapping) = expect_mapping(value, "control config must be a mapping".into())?;
    warn_unknown_keys(
        mapping,
        &["enabled"],
        "control",
        file_id,
        strict,
        diagnostics,
    );
    let enabled = parse_optional::<bool>(mapping.get("enabled"))?;
    Ok(match enabled {
        Some(spanned) => spanned.into_inner(),
        None => true,
    })
}

fn parse_positive_usize(
    value: &yaml_spanned::Spanned<Value>,
    field: &str,
) -> Result<usize, ConfigError> {
    let parsed = parse::<usize>(value)?;
    let parsed = parsed.into_inner();
    if parsed == 0 {
        return Err(ConfigError::InvalidValue {
            message: format!("{field} must be greater than zero"),
            span: value.span().into(),
        });
    }
    Ok(parsed)
}

fn parse_log_limit(
    value: &yaml_spanned::Spanned<Value>,
    field: &str,
) -> Result<LogLimit, ConfigError> {
    if let Some(raw) = value.as_str() {
        let normalized = raw.trim().to_ascii_lowercase();
        if matches!(normalized.as_str(), "unbounded" | "unlimited" | "none") {
            return Ok(LogLimit::Unbounded);
        }
    }
    Ok(LogLimit::bounded(parse_positive_usize(value, field)?))
}

fn warn_overridden_log_alias<F: Copy>(
    mapping: &Mapping,
    memory: &Mapping,
    key: &str,
    file_id: F,
    strict: bool,
    diagnostics: &mut Vec<Diagnostic<F>>,
) {
    let Some(alias_value) = mapping.get(key) else {
        return;
    };
    let Some(memory_value) = memory.get(key) else {
        return;
    };

    diagnostics.push(
        Diagnostic::warning_or_error(strict)
            .with_message(format!("logs.{key} is overridden by logs.memory.{key}"))
            .with_labels(vec![
                Label::primary(file_id, memory_value.span)
                    .with_message("this nested value is used"),
                Label::secondary(file_id, alias_value.span).with_message("this alias is ignored"),
            ]),
    );
}

/// Parse a `logs` config block and apply any specified values to `base`.
fn parse_log_retention<F: Copy>(
    value: Option<&yaml_spanned::Spanned<Value>>,
    base: LogRetention,
    file_id: F,
    strict: bool,
    diagnostics: &mut Vec<Diagnostic<F>>,
) -> Result<LogRetention, ConfigError> {
    let Some(value) = value else {
        return Ok(base);
    };
    let (_span, mapping) = expect_mapping(value, "logs config must be a mapping".into())?;
    warn_unknown_keys(
        mapping,
        &[
            "retained_runs",
            "runs",
            "history",
            "max_lines",
            "max_bytes",
            "memory",
        ],
        "logs",
        file_id,
        strict,
        diagnostics,
    );

    let mut retention = base;
    if let Some(value) = mapping
        .get("retained_runs")
        .or_else(|| mapping.get("runs"))
        .or_else(|| mapping.get("history"))
    {
        retention.disk = DiskLogRetention {
            retained_runs: parse_positive_usize(value, "retained_runs")?,
        };
    }

    let memory = mapping
        .get("memory")
        .map(|memory| {
            expect_mapping(memory, "logs.memory must be a mapping".into())
                .map(|(_span, memory)| memory)
        })
        .transpose()?;

    if let Some(value) = mapping.get("max_lines") {
        retention.memory.max_lines = parse_log_limit(value, "max_lines")?;
    }
    if let Some(value) = mapping.get("max_bytes") {
        retention.memory.max_bytes = parse_log_limit(value, "max_bytes")?;
    }

    if let Some(memory) = memory {
        warn_unknown_keys(
            memory,
            &["max_lines", "max_bytes"],
            "logs.memory",
            file_id,
            strict,
            diagnostics,
        );
        warn_overridden_log_alias(mapping, memory, "max_lines", file_id, strict, diagnostics);
        warn_overridden_log_alias(mapping, memory, "max_bytes", file_id, strict, diagnostics);
        if let Some(value) = memory.get("max_lines") {
            retention.memory.max_lines = parse_log_limit(value, "memory.max_lines")?;
        }
        if let Some(value) = memory.get("max_bytes") {
            retention.memory.max_bytes = parse_log_limit(value, "memory.max_bytes")?;
        }
    }

    Ok(retention)
}

fn invalid_empty_command(raw_command: &str, span: yaml_spanned::spanned::Span) -> ConfigError {
    ConfigError::InvalidCommand {
        command: raw_command.to_string(),
        reason: InvalidCommandReason::EmptyCommand,
        span: span.into(),
    }
}

fn normalize_cmd_exec(
    command: &[Spanned<String>],
    raw_command: &str,
    span: yaml_spanned::spanned::Span,
) -> Result<(Spanned<String>, Vec<Spanned<String>>), ConfigError> {
    // Exec form: ["CMD", prog, arg1, arg2...]
    let Some(prog) = command.get(1).cloned() else {
        // CMD form needs at least one program
        return Err(invalid_empty_command(raw_command, span));
    };
    let args = command
        .get(2..)
        .map(<[yaml_spanned::Spanned<std::string::String>]>::to_vec)
        .unwrap_or_default();
    Ok((prog, args))
}

fn normalize_cmd_shell(
    command: &[Spanned<String>],
    raw_command: &str,
    span: yaml_spanned::spanned::Span,
) -> Result<(Spanned<String>, Vec<Spanned<String>>), ConfigError> {
    // Shell form: ["CMD-SHELL", cmd...]
    // Join everything after index 0 into one string.
    let Some(rest) = command.get(1..) else {
        return Err(invalid_empty_command(raw_command, span));
    };

    let command_string = rest.iter().map(std::convert::AsRef::as_ref).join(" ");
    let Some(span_start) = rest.first().map(|v| v.span.start) else {
        return Err(invalid_empty_command(raw_command, span));
    };
    let Some(span_end) = rest.last().map(|v| v.span.end) else {
        return Err(invalid_empty_command(raw_command, span));
    };

    let cmd_shell_span = command.first().map_or(span, |v| v.span);

    #[cfg(unix)]
    let (prog, args) = (
        Spanned {
            span: cmd_shell_span,
            inner: "sh".to_string(),
        },
        vec![
            Spanned {
                span: cmd_shell_span,
                inner: "-c".to_string(),
            },
            Spanned {
                span: yaml_spanned::spanned::Span {
                    start: span_start,
                    end: span_end,
                },
                inner: command_string,
            },
        ],
    );
    #[cfg(windows)]
    let (prog, args) = (
        Spanned {
            span: cmd_shell_span,
            inner: "cmd.exe".to_string(),
        },
        vec![
            Spanned {
                span: cmd_shell_span,
                inner: "/S".to_string(),
            },
            Spanned {
                span: cmd_shell_span,
                inner: "/C".to_string(),
            },
            Spanned {
                span: yaml_spanned::spanned::Span {
                    start: span_start,
                    end: span_end,
                },
                inner: command_string,
            },
        ],
    );

    Ok((prog, args))
}

pub fn normalize_command(
    command: &[Spanned<String>],
    raw_command: &str,
    span: yaml_spanned::spanned::Span,
) -> Result<(Spanned<String>, Vec<Spanned<String>>), ConfigError> {
    if command.is_empty() {
        return Err(invalid_empty_command(raw_command, span));
    }

    let Some(first) = command.first() else {
        return Err(invalid_empty_command(raw_command, span));
    };

    let (prog, args) = match first.as_str() {
        "CMD" => normalize_cmd_exec(command, raw_command, span)?,
        "CMD-SHELL" => normalize_cmd_shell(command, raw_command, span)?,
        _ => (
            first.clone(),
            command
                .get(1..)
                .map(<[yaml_spanned::Spanned<std::string::String>]>::to_vec)
                .unwrap_or_default(),
        ),
    };

    Ok((prog, args))
}

pub fn parse_command(
    value: &yaml_spanned::Spanned<Value>,
) -> Result<(Spanned<String>, Vec<Spanned<String>>), ConfigError> {
    match value {
        Spanned {
            span,
            inner: Value::String(raw_command),
        } => {
            let trimmed = raw_command.trim_start();
            if let Some(rest) = trimmed
                .strip_prefix("CMD-SHELL")
                .and_then(|s| s.strip_prefix(char::is_whitespace))
            {
                let rest = rest.trim_start();
                if rest.is_empty() {
                    return Err(ConfigError::InvalidCommand {
                        command: raw_command.clone(),
                        reason: InvalidCommandReason::EmptyCommand,
                        span: span.into(),
                    });
                }
                let cmd_shell = Spanned {
                    span: *span,
                    inner: "CMD-SHELL".to_string(),
                };
                let payload = Spanned {
                    span: *span,
                    inner: rest.to_string(),
                };
                return normalize_command(&[cmd_shell, payload], raw_command.as_str(), *span);
            }

            let command = shlex::split(raw_command).ok_or_else(|| ConfigError::InvalidCommand {
                command: raw_command.clone(),
                reason: InvalidCommandReason::FailedToSplit,
                span: span.into(),
            })?;

            // TODO: compute the actual spans by writing our own shlex that tracks positions
            let command = command
                .into_iter()
                .map(|value| Spanned {
                    span: *span,
                    inner: value,
                })
                .collect::<Vec<_>>();

            normalize_command(&command, raw_command.as_str(), *span)
        }
        Spanned {
            span,
            inner: Value::Sequence(command),
        } => {
            let command = command
                .iter()
                .map(|item| {
                    let raw = parse_string_value(item, "command entries must be a scalar")?;
                    Ok::<_, ConfigError>(Spanned {
                        span: item.span,
                        inner: raw,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let raw_command = command.iter().map(std::convert::AsRef::as_ref).join(" ");
            normalize_command(&command, raw_command.as_str(), *span)
        }
        other => Err(ConfigError::UnexpectedType {
            message: "command must be a string or sequence".to_string(),
            expected: vec![Kind::String, Kind::Sequence],
            found: other.kind(),
            span: other.span().into(),
        }),
    }
}

fn parse_health_check<F: Copy>(
    mapping: &yaml_spanned::Mapping,
    defaults: &HealthCheckDefaults,
    file_id: F,
    strict: bool,
    diagnostics: &mut Vec<Diagnostic<F>>,
) -> Result<Option<super::HealthCheck>, ConfigError> {
    mapping
        .get("healthcheck")
        .map(|value| {
            let healthcheck = value
                .as_mapping()
                .ok_or_else(|| ConfigError::UnexpectedType {
                    message: "healthcheck configuration must be a mapping".to_string(),
                    found: value.kind(),
                    expected: vec![Kind::Mapping],
                    span: value.span().into(),
                })?;
            warn_unknown_keys(
                healthcheck,
                KNOWN_HEALTHCHECK_KEYS,
                "healthcheck",
                file_id,
                strict,
                diagnostics,
            );
            let test = match healthcheck.get("test") {
                None => Err(ConfigError::MissingKey {
                    key: "test".to_string(),
                    message: "missing healthcheck test command".to_string(),
                    span: value.span().into(),
                }),
                Some(value) => parse_command(value),
            }?;
            let start_delay = parse_duration(
                healthcheck
                    .get("start_delay")
                    .or_else(|| healthcheck.get("startup_delay"))
                    .or_else(|| healthcheck.get("initial_delay")),
            )?
            .or_else(|| defaults.start_delay.clone());
            let interval =
                parse_duration(healthcheck.get("interval"))?.or_else(|| defaults.interval.clone());
            let retries = parse_optional::<usize>(healthcheck.get("retries"))?
                .or_else(|| defaults.retries.clone());
            let timeout =
                parse_duration(healthcheck.get("timeout"))?.or_else(|| defaults.timeout.clone());
            Ok(super::HealthCheck {
                test,
                start_delay,
                interval,
                timeout,
                retries,
            })
        })
        .transpose()
}

fn parse_healthcheck_defaults<F: Copy>(
    value: &yaml_spanned::Spanned<Value>,
    file_id: F,
    strict: bool,
    diagnostics: &mut Vec<Diagnostic<F>>,
) -> Result<HealthCheckDefaults, ConfigError> {
    let Some(value) = value.get("healthcheck") else {
        return Ok(HealthCheckDefaults::default());
    };
    let (_span, mapping) = expect_mapping(value, "healthcheck defaults must be a mapping".into())?;
    warn_unknown_keys(
        mapping,
        KNOWN_HEALTHCHECK_TIMING_KEYS,
        "healthcheck defaults",
        file_id,
        strict,
        diagnostics,
    );
    Ok(HealthCheckDefaults {
        start_delay: parse_duration(
            mapping
                .get("start_delay")
                .or_else(|| mapping.get("startup_delay"))
                .or_else(|| mapping.get("initial_delay")),
        )?,
        interval: parse_duration(mapping.get("interval"))?,
        timeout: parse_duration(mapping.get("timeout"))?,
        retries: parse_optional::<usize>(mapping.get("retries"))?,
    })
}

#[derive(Clone, Copy)]
struct ServiceDefaults<'a> {
    log_retention: LogRetention,
    restart_policy: &'a RestartPolicy,
    healthcheck: &'a HealthCheckDefaults,
}

fn parse_service<F: Copy>(
    value: &yaml_spanned::Spanned<Value>,
    name: &yaml_spanned::Spanned<String>,
    defaults: ServiceDefaults<'_>,
    file_id: F,
    strict: bool,
    diagnostics: &mut Vec<Diagnostic<F>>,
) -> Result<Service, ConfigError> {
    let (span, mapping) = expect_mapping(value, "service config must be a mapping".into())?;
    warn_unknown_keys(
        mapping,
        KNOWN_SERVICE_KEYS,
        "service",
        file_id,
        strict,
        diagnostics,
    );
    let name = parse_optional::<String>(mapping.get("name"))?.unwrap_or_else(|| name.clone());
    let color = parse_optional::<bool>(mapping.get("color"))?;
    let working_dir = mapping
        .get("working_dir")
        .or_else(|| mapping.get("cwd"))
        .or_else(|| mapping.get("directory"));
    let working_dir = parse_optional::<String>(working_dir)?;
    let command = match mapping.get("command") {
        None => Err(ConfigError::MissingKey {
            key: "command".to_string(),
            message: "missing command".to_string(),
            span: span.into(),
        }),
        Some(value) => parse_command(value),
    }?;
    let healthcheck =
        parse_health_check(mapping, defaults.healthcheck, file_id, strict, diagnostics)?;

    let env_file = parse_env_file(mapping)?;
    let environment = parse_environment(mapping)?;
    let depends_on = parse_depends_on(mapping)?;
    let ports = parse_ports(mapping)?;
    let restart = parse_restart(mapping)?;
    let restart_policy = restart
        .clone()
        .unwrap_or_else(|| defaults.restart_policy.clone());
    let log_retention = parse_log_retention(
        mapping.get("logs"),
        defaults.log_retention,
        file_id,
        strict,
        diagnostics,
    )?;

    Ok(Service {
        name,
        command,
        working_dir,
        env_file,
        environment,
        depends_on,
        healthcheck,
        ports,
        restart,
        restart_policy,
        color,
        log_retention,
    })
}

fn parse_services<F: Copy>(
    value: &yaml_spanned::Spanned<Value>,
    defaults: ServiceDefaults<'_>,
    file_id: F,
    strict: bool,
    diagnostics: &mut Vec<Diagnostic<F>>,
) -> Result<IndexMap<Spanned<String>, Service>, ConfigError> {
    match value.get("services") {
        None => {
            diagnostics.push(
                Diagnostic::warning_or_error(strict)
                    .with_message("no services defined")
                    .with_labels(vec![
                        Label::primary(file_id, value.span)
                            .with_message("config defines zero services to supervise"),
                    ]),
            );
            Ok(IndexMap::default())
        }
        Some(value) => {
            let services = value
                .as_mapping()
                .ok_or_else(|| ConfigError::UnexpectedType {
                    message: "services must be a mapping".to_string(),
                    found: value.kind(),
                    expected: vec![Kind::Mapping],
                    span: value.span().into(),
                })?;

            let services = services
                .iter()
                .map(|(name, service)| {
                    let name = parse::<String>(name)?;
                    let service =
                        parse_service(service, &name, defaults, file_id, strict, diagnostics)?;
                    Ok::<_, ConfigError>((name, service))
                })
                .collect::<Result<Vec<(Spanned<String>, Service)>, _>>()?
                .into_iter()
                .collect::<IndexMap<Spanned<String>, Service>>();
            Ok(services)
        }
    }
}

pub fn parse_config<F: Copy + PartialEq>(
    // name: Spanned<String>,
    // config_span: Option<yaml_spanned::spanned::Span>,
    value: &yaml_spanned::Spanned<Value>,
    file_id: F,
    strict_override: Option<bool>,
    diagnostics: &mut Vec<Diagnostic<F>>,
) -> Result<Config, ConfigError> {
    let strict_config = super::parse_strict(value)?;
    let strict = strict_override.or(strict_config).unwrap_or(false);
    let name = parse_optional::<String>(value.get("name"))?.map(Spanned::into_inner);
    let ui_config = parse_ui_config(value, file_id, strict, diagnostics)?;
    let control_enabled = parse_control_enabled(value, file_id, strict, diagnostics)?;
    let restart_policy = value
        .get("restart")
        .map(parse_restart_value)
        .transpose()?
        .unwrap_or_default();
    let healthcheck_defaults = parse_healthcheck_defaults(value, file_id, strict, diagnostics)?;
    let log_retention = parse_log_retention(
        value.get("logs"),
        LogRetention::default(),
        file_id,
        strict,
        diagnostics,
    )?;
    let services = parse_services(
        value,
        ServiceDefaults {
            log_retention,
            restart_policy: &restart_policy,
            healthcheck: &healthcheck_defaults,
        },
        file_id,
        strict,
        diagnostics,
    )?;
    // let template_engine = parse_optional::<model::TemplateEngine>(
    //     value.get("engine").or_else(|| value.get("template_engine")),
    // )?;
    // let check_templates =
    //     parse_optional::<bool>(value.get("check_templates"))?.map(Spanned::into_inner);
    // let inputs = parse_inputs(value, config_span, file_id, strict, diagnostics)?;
    // let outputs = parse_outputs(value, config_span, file_id, strict, diagnostics)?;

    // let config = Config { version, services };
    Ok(Config {
        name,
        ui_config,
        control_enabled,
        log_retention,
        restart_policy,
        healthcheck_defaults,
        services,
    })
}

#[cfg(test)]
mod tests {

    use crate::{LogLimit, LogRetention, config};
    use codespan_reporting::diagnostic::Diagnostic;
    use color_eyre::eyre;
    use indoc::indoc;
    use similar_asserts::assert_eq;
    use std::path::Path;

    fn get_service<'a>(cfg: &'a config::Config, name: &str) -> eyre::Result<&'a config::Service> {
        cfg.services
            .iter()
            .find(|(k, _)| k.as_ref() == name)
            .map(|(_, v)| v)
            .ok_or_else(|| eyre::eyre!("missing service {name}"))
    }

    #[test]
    fn env_file_accepts_string_mapping_and_sequence_forms() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              app_string:
                command: ["sh", "-c", "true"]
                env_file: ".env"
              app_mapping:
                command: ["sh", "-c", "true"]
                env_file:
                  path: ".env"
              app_sequence:
                command: ["sh", "-c", "true"]
                env_file:
                  - ".env"
        "#};

        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = config::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)?;

        let s1 = get_service(&parsed.config, "app_string")?;
        assert_eq!(s1.env_file.len(), 1);
        assert_eq!(
            s1.env_file.first().map(|v| v.path.as_ref().as_str()),
            Some(".env")
        );

        let s2 = get_service(&parsed.config, "app_mapping")?;
        assert_eq!(s2.env_file.len(), 1);
        assert_eq!(
            s2.env_file.first().map(|v| v.path.as_ref().as_str()),
            Some(".env")
        );

        let s3 = get_service(&parsed.config, "app_sequence")?;
        assert_eq!(s3.env_file.len(), 1);
        assert_eq!(
            s3.env_file.first().map(|v| v.path.as_ref().as_str()),
            Some(".env")
        );

        Ok(())
    }

    #[test]
    fn depends_on_condition_parses_and_invalid_condition_is_error() -> eyre::Result<()> {
        let yaml_ok = indoc! {r#"
            version: 1
            services:
              app:
                command: ["sh", "-c", "true"]
                depends_on:
                  - name: db
                    condition: healthy
              db:
                command: ["sh", "-c", "true"]
        "#};

        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = config::from_str(yaml_ok, Path::new("."), 0, None, &mut diagnostics)?;
        let app = get_service(&parsed.config, "app")?;
        assert_eq!(app.depends_on.len(), 1);
        let Some(dep) = app.depends_on.first() else {
            return Err(eyre::eyre!("missing depends_on entry"));
        };
        assert_eq!(dep.name.as_ref(), "db");
        assert_eq!(
            dep.condition.as_ref().map(|c| *c.as_ref()),
            Some(config::DependencyCondition::Healthy)
        );

        let yaml_bad = indoc! {r#"
            version: 1
            services:
              app:
                command: ["sh", "-c", "true"]
                depends_on:
                  - name: db
                    condition: totally_not_a_condition
              db:
                command: ["sh", "-c", "true"]
        "#};

        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        match config::from_str(yaml_bad, Path::new("."), 0, None, &mut diagnostics) {
            Ok(_) => return Err(eyre::eyre!("expected error")),
            Err(config::ConfigError::Serde { .. }) => {}
            Err(other) => return Err(eyre::eyre!("expected serde error, got {other}")),
        }

        Ok(())
    }

    #[test]
    fn cmd_shell_string_preserves_quoting_in_payload() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              app:
                command: ["sh", "-c", "true"]
                healthcheck:
                  test: "CMD-SHELL echo \"a b\""
        "#};

        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = config::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)?;
        let svc = get_service(&parsed.config, "app")?;
        let Some(hc) = svc.healthcheck.as_ref() else {
            return Err(eyre::eyre!("missing healthcheck"));
        };
        #[cfg(unix)]
        {
            assert_eq!(hc.test.0.as_ref(), "sh");
            assert_eq!(hc.test.1.first().map(|v| v.as_ref().as_str()), Some("-c"));
            assert_eq!(
                hc.test.1.get(1).map(|v| v.as_ref().as_str()),
                Some("echo \"a b\"")
            );
        }
        #[cfg(windows)]
        {
            assert_eq!(hc.test.0.as_ref(), "cmd.exe");
            assert_eq!(hc.test.1.first().map(|v| v.as_ref().as_str()), Some("/S"));
            assert_eq!(hc.test.1.get(1).map(|v| v.as_ref().as_str()), Some("/C"));
            assert_eq!(
                hc.test.1.get(2).map(|v| v.as_ref().as_str()),
                Some("echo \"a b\"")
            );
        }
        Ok(())
    }

    #[test]
    fn unknown_service_key_warns() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              app:
                command: ["sh", "-c", "true"]
                dependson:
                  - db
              db:
                command: ["sh", "-c", "true"]
        "#};

        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let _ = config::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)?;
        assert!(
            diagnostics
                .iter()
                .any(|d| d.message.contains("unknown service field `dependson`")),
            "expected an unknown-field diagnostic, got: {:?}",
            diagnostics.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[test]
    fn bare_on_failure_is_unlimited() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              app:
                command: ["sh", "-c", "true"]
                restart: on-failure
        "#};

        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = config::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)?;
        let app = get_service(&parsed.config, "app")?;
        assert!(
            matches!(
                app.restart,
                Some(crate::service::RestartPolicy::OnFailure { max_attempts: None })
            ),
            "expected unlimited on-failure, got {:?}",
            app.restart
        );
        Ok(())
    }

    #[test]
    fn global_restart_and_healthcheck_defaults_are_inherited() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            restart: unless-stopped
            healthcheck:
              start_delay: "1s"
              interval: "30s"
              timeout: "5s"
              retries: 3
            services:
              app:
                command: ["sh", "-c", "true"]
                healthcheck:
                  test: ["CMD", "true"]
                  timeout: "1s"
              worker:
                command: ["sh", "-c", "true"]
                restart: never
        "#};

        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = config::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)?;
        assert_eq!(
            parsed.config.restart_policy,
            crate::service::RestartPolicy::UnlessStopped
        );
        assert_eq!(
            parsed
                .config
                .healthcheck_defaults
                .interval
                .as_ref()
                .map(|value| value.as_ref().as_secs()),
            Some(30)
        );

        let app = get_service(&parsed.config, "app")?;
        assert_eq!(app.restart, None);
        assert_eq!(
            app.restart_policy,
            crate::service::RestartPolicy::UnlessStopped
        );
        let Some(healthcheck) = app.healthcheck.as_ref() else {
            return Err(eyre::eyre!("missing healthcheck"));
        };
        assert_eq!(
            healthcheck
                .start_delay
                .as_ref()
                .map(|value| value.as_ref().as_secs()),
            Some(1)
        );
        assert_eq!(
            healthcheck
                .interval
                .as_ref()
                .map(|value| value.as_ref().as_secs()),
            Some(30)
        );
        assert_eq!(
            healthcheck
                .timeout
                .as_ref()
                .map(|value| value.as_ref().as_secs()),
            Some(1)
        );
        assert_eq!(
            healthcheck
                .retries
                .as_ref()
                .map(std::convert::AsRef::as_ref),
            Some(&3)
        );

        let worker = get_service(&parsed.config, "worker")?;
        assert_eq!(worker.restart, Some(crate::service::RestartPolicy::Never));
        assert_eq!(worker.restart_policy, crate::service::RestartPolicy::Never);
        assert_eq!(worker.healthcheck, None);
        Ok(())
    }

    #[test]
    fn parses_top_level_name_and_control() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            name: my-project
            ui:
              pretty_json_logs: false
            control:
              enabled: false
            logs:
              retained_runs: 5
              memory:
                max_lines: 2000
                max_bytes: 1048576
            services:
              app:
                command: ["sh", "-c", "true"]
              worker:
                command: ["sh", "-c", "true"]
                logs:
                  retained_runs: 9
                  memory:
                    max_lines: unbounded
        "#};

        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = config::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)?;
        assert_eq!(parsed.config.name.as_deref(), Some("my-project"));
        assert!(!parsed.config.ui_config.pretty_json_logs);
        assert!(!parsed.config.control_enabled);
        assert_eq!(parsed.config.log_retention.disk.retained_runs, 5);
        assert_eq!(
            parsed.config.log_retention.memory.max_lines,
            LogLimit::Bounded(2000)
        );
        assert_eq!(
            parsed.config.log_retention.memory.max_bytes,
            LogLimit::Bounded(1_048_576)
        );
        let app = get_service(&parsed.config, "app")?;
        assert_eq!(app.log_retention, parsed.config.log_retention);
        let worker = get_service(&parsed.config, "worker")?;
        assert_eq!(worker.log_retention.disk.retained_runs, 9);
        assert_eq!(worker.log_retention.memory.max_lines, LogLimit::Unbounded);
        assert_eq!(
            worker.log_retention.memory.max_bytes,
            LogLimit::Bounded(1_048_576)
        );

        // Defaults: no name, control enabled.
        let yaml = indoc! {r#"
            version: 1
            services:
              app:
                command: ["sh", "-c", "true"]
        "#};
        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = config::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)?;
        assert_eq!(parsed.config.name, None);
        assert!(parsed.config.ui_config.pretty_json_logs);
        assert!(parsed.config.control_enabled);
        assert_eq!(parsed.config.log_retention, LogRetention::default());
        assert_eq!(
            parsed.config.restart_policy,
            crate::service::RestartPolicy::Never
        );
        assert_eq!(
            parsed.config.healthcheck_defaults,
            config::HealthCheckDefaults::default()
        );
        Ok(())
    }

    #[test]
    fn warns_when_logs_memory_overrides_aliases() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            logs:
              max_lines: 100
              max_bytes: 1000
              memory:
                max_lines: 200
                max_bytes: 2000
            services:
              app:
                command: ["sh", "-c", "true"]
        "#};

        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = config::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)?;

        assert_eq!(
            parsed.config.log_retention.memory.max_lines,
            LogLimit::Bounded(200)
        );
        assert_eq!(
            parsed.config.log_retention.memory.max_bytes,
            LogLimit::Bounded(2000)
        );
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("logs.max_lines is overridden by logs.memory.max_lines")
        }));
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("logs.max_bytes is overridden by logs.memory.max_bytes")
        }));
        Ok(())
    }

    #[test]
    fn cmd_shell_sequence_joins_items_with_spaces() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              app:
                command: ["sh", "-c", "true"]
                healthcheck:
                  test: ["CMD-SHELL", "echo", "a b"]
        "#};

        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = config::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)?;
        let svc = get_service(&parsed.config, "app")?;
        let Some(hc) = svc.healthcheck.as_ref() else {
            return Err(eyre::eyre!("missing healthcheck"));
        };
        #[cfg(unix)]
        {
            assert_eq!(hc.test.0.as_ref(), "sh");
            assert_eq!(hc.test.1.first().map(|v| v.as_ref().as_str()), Some("-c"));
            assert_eq!(
                hc.test.1.get(1).map(|v| v.as_ref().as_str()),
                Some("echo a b")
            );
        }
        #[cfg(windows)]
        {
            assert_eq!(hc.test.0.as_ref(), "cmd.exe");
            assert_eq!(hc.test.1.first().map(|v| v.as_ref().as_str()), Some("/S"));
            assert_eq!(hc.test.1.get(1).map(|v| v.as_ref().as_str()), Some("/C"));
            assert_eq!(
                hc.test.1.get(2).map(|v| v.as_ref().as_str()),
                Some("echo a b")
            );
        }
        Ok(())
    }
}
