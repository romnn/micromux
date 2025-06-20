use super::{Config, ConfigError, Service, UiConfig, parse, parse_duration, parse_optional};
use crate::{
    config::InvalidCommandReason,
    diagnostics::{self, DiagnosticExt, DisplayRepr, Span},
};
use codespan_reporting::diagnostic::{Diagnostic, Label};
use indexmap::IndexMap;
use itertools::Itertools;
use std::path::{Path, PathBuf};
use yaml_spanned::{Mapping, Sequence, Spanned, Value, value::Kind};

pub fn expect_sequence(value: &yaml_spanned::Spanned<Value>) -> Result<&Sequence, ConfigError> {
    value
        .as_sequence()
        .ok_or_else(|| ConfigError::UnexpectedType {
            message: "expected sequence".to_string(),
            expected: vec![Kind::Sequence],
            found: value.kind(),
            span: value.span().into(),
        })
}

pub fn expect_mapping(
    value: &yaml_spanned::Spanned<Value>,
) -> Result<(&yaml_spanned::spanned::Span, &Mapping), ConfigError> {
    let mapping = value
        .as_mapping()
        .ok_or_else(|| ConfigError::UnexpectedType {
            message: "expected mapping".to_string(),
            expected: vec![Kind::Mapping],
            found: value.kind(),
            span: value.span().into(),
        })?;
    Ok((value.span(), mapping))
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
    let (_span, mapping) = expect_mapping(value)?;
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
            // let test: Option<String> = None;
            // test.map(f)
            let raw_command = command.iter().join(" ");
            let command =
                command
                    .into_iter()
                    .map(|Spanned { span, inner }| {
                        let inner = inner.as_string().cloned().ok_or_else(|| {
                            ConfigError::UnexpectedType {
                                message: "command arguments must be strings".to_string(),
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
            let interval = parse_duration(healthcheck.get("interval"))?;
            let retries = parse_optional::<usize>(healthcheck.get("retries"))?;
            let timeout = parse_duration(healthcheck.get("timeout"))?;
            Ok::<_, ConfigError>(super::HealthCheck {
                test,
                interval,
                retries,
                timeout,
            })
        })
        .transpose()
}

pub fn parse_service<F>(
    value: &yaml_spanned::Spanned<Value>,
    _file_id: F,
    _strict: bool,
    _diagnostics: &mut Vec<Diagnostic<F>>,
) -> Result<Service, ConfigError> {
    let (span, mapping) = expect_mapping(value)?;
    let command = match mapping.get("command") {
        None => Err(ConfigError::MissingKey {
            key: "command".to_string(),
            message: "missing command".to_string(),
            span: span.into(),
        }),
        Some(value) => parse_command(value),
    }?;
    let healthcheck = parse_health_check(mapping)?;
    dbg!(&healthcheck);
    Ok(Service {
        command,
        env_file: vec![],
        environment: IndexMap::default(),
        depends_on: vec![],
        healthcheck,
        restart: None,
        ports: vec![],
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
                    let service = parse_service(service, file_id, strict, diagnostics)?;
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
