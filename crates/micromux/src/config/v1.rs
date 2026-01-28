use super::{Config, ConfigError, Service, UiConfig, parse, parse_duration, parse_optional};
use crate::{
    config::InvalidCommandReason,
    diagnostics::{self, DiagnosticExt, DisplayRepr, Span},
    service::RestartPolicy,
};
use codespan_reporting::diagnostic::{Diagnostic, Label};
use indexmap::IndexMap;
use itertools::Itertools;
use std::path::{Path, PathBuf};
use yaml_spanned::{Mapping, Sequence, Spanned, Value, value::Kind};

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
    for (k, v) in env_mapping.iter() {
        let key = parse::<String>(k)?;
        let raw = parse_string_value(v, "environment values must be scalar")?;
        env.insert(
            key,
            Spanned {
                span: v.span.clone(),
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
                        span: span.clone(),
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
                        span: path_span.clone(),
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
            for item in seq.iter() {
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
    for item in seq.iter() {
        match item {
            Spanned {
                span,
                inner: Value::String(name),
            } => {
                deps.push(super::Dependency {
                    name: Spanned {
                        span: span.clone(),
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
    for item in seq.iter() {
        let raw = parse_string_value(item, "ports entries must be a scalar")?;
        ports.push(Spanned {
            span: item.span.clone(),
            inner: raw,
        });
    }
    Ok(ports)
}

fn parse_restart(mapping: &yaml_spanned::Mapping) -> Result<Option<RestartPolicy>, ConfigError> {
    let Some(value) = mapping.get("restart") else {
        return Ok(None);
    };
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
                let attempts = if rest.is_empty() {
                    1
                } else {
                    rest.parse::<usize>()
                        .map_err(|_| ConfigError::InvalidValue {
                            message: format!("invalid restart policy `{raw}`"),
                            span: value.span().into(),
                        })?
                };
                RestartPolicy::OnFailure {
                    remaining_attempts: attempts,
                }
            } else {
                return Err(ConfigError::InvalidValue {
                    message: format!("invalid restart policy `{raw}`"),
                    span: value.span().into(),
                });
            }
        }
    };
    Ok(Some(policy))
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

pub fn parse_ui_config<F>(
    value: &yaml_spanned::Spanned<Value>,
    _file_id: F,
    _strict: bool,
    _diagnostics: &mut Vec<Diagnostic<F>>,
) -> Result<UiConfig, ConfigError> {
    let Some(value) = value.get("ui") else {
        return Ok(UiConfig::default());
    };
    let (_span, mapping) = expect_mapping(value, "ui config must be a mapping".into())?;
    let width = parse_optional::<usize>(mapping.get("width"))?;
    Ok(UiConfig { width })
}

pub fn normalize_command(
    command: Vec<Spanned<String>>,
    raw_command: &str,
    span: yaml_spanned::spanned::Span,
) -> Result<(Spanned<String>, Vec<Spanned<String>>), ConfigError> {
    if command.is_empty() {
        return Err(ConfigError::InvalidCommand {
            command: raw_command.to_string(),
            reason: InvalidCommandReason::EmptyCommand,
            span: span.into(),
        });
    }

    let (prog, args) = match command[0].as_str() {
        "CMD" => {
            // Exec form: ["CMD", prog, arg1, arg2...]
            if command.len() < 2 {
                // CMD form needs at least one program
                return Err(ConfigError::InvalidCommand {
                    command: raw_command.to_string(),
                    reason: InvalidCommandReason::EmptyCommand,
                    span: span.into(),
                });
            }
            let prog = command[1].clone();
            let args = command[2..].to_vec();
            (prog, args)
        }
        "CMD-SHELL" => {
            // Shell form: ["CMD-SHELL", cmd...]
            // Join everything after index 0 into one string:
            let command_string = command[1..].join(" ");
            let cmd_shell_span = &command[0].span;

            #[cfg(unix)]
            let (prog, args) = (
                Spanned {
                    span: cmd_shell_span.clone().into(),
                    inner: "sh".to_string(),
                },
                vec![
                    Spanned {
                        span: cmd_shell_span.clone().into(),
                        inner: "-c".to_string(),
                    },
                    Spanned {
                        span: yaml_spanned::spanned::Span {
                            start: command[1].span.start,
                            end: command[command.len() - 1].span.end,
                        },
                        inner: command_string,
                    },
                ],
            );
            #[cfg(windows)]
            let (prog, args) = (
                Spanned {
                    span: cmd_shell_span.clone().into(),
                    inner: "cmd.exe".to_string(),
                },
                vec![
                    Spanned {
                        span: cmd_shell_span.clone().into(),
                        inner: "/S".to_string(),
                    },
                    Spanned {
                        span: cmd_shell_span.clone().into(),
                        inner: "/C".to_string(),
                    },
                    Spanned {
                        span: yaml_spanned::spanned::Span {
                            start: command[1].span.start,
                            end: command[command.len() - 1].span.end,
                        },
                        inner: command_string,
                    },
                ],
            );
            (prog, args)
        }
        _ => {
            let prog = command[0].clone();
            let args = command[1..].to_vec();
            (prog, args)
        }
    };

    Ok((prog, args))
}

fn parse_command(
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
                    span: span.clone(),
                    inner: "CMD-SHELL".to_string(),
                };
                let payload = Spanned {
                    span: span.clone(),
                    inner: rest.to_string(),
                };
                return normalize_command(vec![cmd_shell, payload], raw_command.as_str(), *span);
            }

            let command =
                shlex::split(&raw_command).ok_or_else(|| ConfigError::InvalidCommand {
                    command: raw_command.clone(),
                    reason: InvalidCommandReason::FailedToSplit,
                    span: span.into(),
                })?;

            // TODO: compute the actual spans by writing our own shlex that tracks positions
            let command = command
                .into_iter()
                .map(|value| Spanned {
                    span: span.clone(),
                    inner: value,
                })
                .collect();

            normalize_command(command, raw_command.as_str(), *span)

            // Ok(Spanned {
            //     span: *span,
            //     inner: command,
            // })
        }
        Spanned {
            span,
            inner: Value::Sequence(command),
        } => {
            let raw_command = command.iter().join(" ");
            let command =
                command
                    .into_iter()
                    .map(|Spanned { span, inner }| {
                        let inner = inner.as_string().cloned().ok_or_else(|| {
                            ConfigError::UnexpectedType {
                                message: "command arguments must be string".to_string(),
                                expected: vec![Kind::String],
                                found: inner.kind(),
                                span: span.into(),
                            }
                        })?;
                        Ok::<_, ConfigError>(Spanned {
                            span: span.clone(),
                            inner,
                        })
                    })
                    // .map(|Spanned { span, inner }| {
                    //     inner
                    //         .as_string()
                    //         .cloned()
                    //         .ok_or_else(|| ConfigError::UnexpectedType {
                    //             message: "command arguments must be strings".to_string(),
                    //             expected: vec![Kind::String],
                    //             found: inner.kind(),
                    //             span: span.into(),
                    //         })
                    // })
                    .collect::<Result<Vec<_>, ConfigError>>()?;
            normalize_command(command, raw_command.as_str(), *span)
            // Ok(command)
            // Ok(Spanned {
            //     span: *span,
            //     inner: command,
            // })
        }
        Spanned { span, inner } => Err(ConfigError::UnexpectedType {
            message: "command must be a string or sequence of strings".to_string(),
            found: inner.kind(),
            expected: vec![Kind::Mapping],
            span: span.into(),
        }),
    }
}

pub fn parse_health_check(
    mapping: &yaml_spanned::Mapping,
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
            )?;
            let interval = parse_duration(healthcheck.get("interval"))?;
            let retries = parse_optional::<usize>(healthcheck.get("retries"))?;
            let timeout = parse_duration(healthcheck.get("timeout"))?;
            Ok::<_, ConfigError>(super::HealthCheck {
                test,
                start_delay,
                interval,
                retries,
                timeout,
            })
        })
        .transpose()
}

pub fn parse_service<F>(
    value: &yaml_spanned::Spanned<Value>,
    name: &yaml_spanned::Spanned<String>,
    _file_id: F,
    _strict: bool,
    _diagnostics: &mut Vec<Diagnostic<F>>,
) -> Result<Service, ConfigError> {
    let (span, mapping) = expect_mapping(value, "service config must be a mapping".into())?;
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
    let healthcheck = parse_health_check(mapping)?;

    let env_file = parse_env_file(mapping)?;
    let environment = parse_environment(mapping)?;
    let depends_on = parse_depends_on(mapping)?;
    let ports = parse_ports(mapping)?;
    let restart = parse_restart(mapping)?;

    Ok(Service {
        name,
        command,
        working_dir,
        env_file,
        environment,
        depends_on,
        healthcheck,
        restart,
        ports,
        color,
    })
}

pub fn parse_services<F: Copy>(
    value: &yaml_spanned::Spanned<Value>,
    file_id: F,
    strict: bool,
    diagnostics: &mut Vec<Diagnostic<F>>,
) -> Result<IndexMap<Spanned<String>, Service>, ConfigError> {
    match value.get("services") {
        None => {
            // let diagnostic = Diagnostic::warning_or_error(strict)
            //     .with_message("empty languages")
            //     .with_labels(vec![Label::primary(file_id, value.span).with_message(
            //         "no languages specified - no JSON translation file will be generated",
            //     )]);
            // diagnostics.push(diagnostic);
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
                    let service = parse_service(service, &name, file_id, strict, diagnostics)?;
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
    // let strict_config = parse_optional::<bool>(value.get("strict"))?.map(Spanned::into_inner);
    let strict = strict_override.unwrap_or(false);
    let ui_config = parse_ui_config(value, file_id, strict, diagnostics)?;
    let services = parse_services(value, file_id, strict, diagnostics)?;
    // let template_engine = parse_optional::<model::TemplateEngine>(
    //     value.get("engine").or_else(|| value.get("template_engine")),
    // )?;
    // let check_templates =
    //     parse_optional::<bool>(value.get("check_templates"))?.map(Spanned::into_inner);
    // let inputs = parse_inputs(value, config_span, file_id, strict, diagnostics)?;
    // let outputs = parse_outputs(value, config_span, file_id, strict, diagnostics)?;

    // let config = Config { version, services };
    Ok(Config {
        ui_config,
        services,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use codespan_reporting::diagnostic::Diagnostic;
    use color_eyre::eyre;
    use std::path::Path;

    fn get_service<'a>(cfg: &'a config::Config, name: &str) -> &'a config::Service {
        cfg.services
            .iter()
            .find(|(k, _)| k.as_ref() == name)
            .map(|(_, v)| v)
            .unwrap()
    }

    #[test]
    fn env_file_accepts_string_mapping_and_sequence_forms() -> eyre::Result<()> {
        let yaml = r#"
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
"#;

        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = config::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)?;

        let s1 = get_service(&parsed.config, "app_string");
        assert_eq!(s1.env_file.len(), 1);
        assert_eq!(s1.env_file[0].path.as_ref(), ".env");

        let s2 = get_service(&parsed.config, "app_mapping");
        assert_eq!(s2.env_file.len(), 1);
        assert_eq!(s2.env_file[0].path.as_ref(), ".env");

        let s3 = get_service(&parsed.config, "app_sequence");
        assert_eq!(s3.env_file.len(), 1);
        assert_eq!(s3.env_file[0].path.as_ref(), ".env");

        Ok(())
    }

    #[test]
    fn depends_on_condition_parses_and_invalid_condition_is_error() {
        let yaml_ok = r#"
version: 1
services:
  app:
    command: ["sh", "-c", "true"]
    depends_on:
      - name: db
        condition: healthy
  db:
    command: ["sh", "-c", "true"]
"#;

        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = config::from_str(yaml_ok, Path::new("."), 0, None, &mut diagnostics).unwrap();
        let app = get_service(&parsed.config, "app");
        assert_eq!(app.depends_on.len(), 1);
        let dep = &app.depends_on[0];
        assert_eq!(dep.name.as_ref(), "db");
        assert_eq!(
            dep.condition.as_ref().map(|c| *c.as_ref()),
            Some(config::DependencyCondition::ServiceHealthy)
        );

        let yaml_bad = r#"
version: 1
services:
  app:
    command: ["sh", "-c", "true"]
    depends_on:
      - name: db
        condition: totally_not_a_condition
  db:
    command: ["sh", "-c", "true"]
"#;

        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let err =
            config::from_str(yaml_bad, Path::new("."), 0, None, &mut diagnostics).unwrap_err();
        match err {
            config::ConfigError::Serde { .. } => {}
            other => panic!("expected serde error, got {other:?}"),
        }
    }

    #[test]
    fn cmd_shell_string_preserves_quoting_in_payload() -> eyre::Result<()> {
        let yaml = r#"
version: 1
services:
  app:
    command: ["sh", "-c", "true"]
    healthcheck:
      test: "CMD-SHELL echo \"a b\""
"#;

        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = config::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)?;
        let svc = get_service(&parsed.config, "app");
        let hc = svc.healthcheck.as_ref().unwrap();
        assert_eq!(hc.test.0.as_ref(), "sh");
        assert_eq!(hc.test.1[0].as_ref(), "-c");
        assert_eq!(hc.test.1[1].as_ref(), "echo \"a b\"");
        Ok(())
    }

    #[test]
    fn cmd_shell_sequence_joins_items_with_spaces() -> eyre::Result<()> {
        let yaml = r#"
version: 1
services:
  app:
    command: ["sh", "-c", "true"]
    healthcheck:
      test: ["CMD-SHELL", "echo", "a b"]
"#;

        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = config::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)?;
        let svc = get_service(&parsed.config, "app");
        let hc = svc.healthcheck.as_ref().unwrap();
        assert_eq!(hc.test.0.as_ref(), "sh");
        assert_eq!(hc.test.1[0].as_ref(), "-c");
        assert_eq!(hc.test.1[1].as_ref(), "echo a b");
        Ok(())
    }
}
