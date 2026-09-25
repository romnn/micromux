use crate::{
    config::{self},
    diagnostics::{Span, ToDiagnostics},
    env,
    model::LogRetention,
    scheduler::ServiceID,
    spec::{DependencySpec, HealthcheckSpec, ServiceOrigin, ServiceSpec},
};
use codespan_reporting::diagnostic::{Diagnostic, Label};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::Arc;

/// Errors from materializing a runnable service definition.
///
/// Every variant names its service and carries the span of the configured value, so the CLI
/// and `validate_config` can point at the source while the config-reload path, which reports
/// through a single rendered line, still gets a self-contained message.
/// Messages include their cause for that same reason.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A configured value references a variable that cannot be substituted.
    #[error("cannot resolve {site} of service `{service}`: {source}")]
    Interpolation {
        /// Service the value belongs to.
        service: ServiceID,
        /// Which configured value failed.
        site: Site,
        /// Span of the value in the config file.
        span: Span,
        /// The failing reference.
        #[source]
        source: env::InterpolationError,
    },
    /// An environment file could not be read or parsed.
    #[error("cannot load env_file[{index}] of service `{service}`: {source}")]
    EnvFile {
        /// Service the file belongs to.
        service: ServiceID,
        /// Position of the entry in the `env_file` list.
        index: usize,
        /// Span of the entry's path in the config file.
        span: Span,
        /// Underlying read or parse error, which names the resolved path.
        #[source]
        source: env::Error,
    },
    /// An advertised port is not a valid TCP/UDP port number.
    #[error("invalid port `{port}` in ports[{index}] of service `{service}`: {source}")]
    InvalidPort {
        /// Service the port belongs to.
        service: ServiceID,
        /// Position of the entry in the `ports` list.
        index: usize,
        /// Span of the entry in the config file.
        span: Span,
        /// Expanded port value.
        port: String,
        /// Underlying integer parse error.
        #[source]
        source: std::num::ParseIntError,
    },
    /// The configured working directory could not be opened or is not a directory.
    #[error("cannot use working_dir of service `{service}`: {source}")]
    WorkingDirectory {
        /// Service the directory belongs to.
        service: ServiceID,
        /// Span of the `working_dir` value in the config file.
        span: Span,
        /// Underlying filesystem error, which names the resolved path.
        #[source]
        source: WorkingDirectoryError,
    },
}

/// A working directory could not be opened or is not a directory.
#[derive(Debug, thiserror::Error)]
#[error("failed to access working directory {}: {source}", path.display())]
pub struct WorkingDirectoryError {
    /// Resolved working-directory path.
    pub path: PathBuf,
    /// Underlying filesystem error.
    #[source]
    pub source: std::io::Error,
}

/// The configured value an interpolation error refers to.
///
/// Displays as the path of the value under its service, such as `ports[1]` or
/// `env_file[0].path`, so a message can name the exact key an author has to change.
/// Argv indexes count elements after the command was split and any `CMD-SHELL` prefix
/// lowered, which is why the span, not the index, is what locates a string-form command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Site {
    /// The `working_dir` value.
    WorkingDir,
    /// The path of the `env_file` entry at `index`.
    EnvFilePath {
        /// Position in the `env_file` list.
        index: usize,
    },
    /// The value of `key` inside the loaded `env_file` entry at `index`.
    EnvFileValue {
        /// Position in the `env_file` list.
        index: usize,
        /// Resolved path of the file.
        path: Box<Path>,
        /// Dotenv key whose value failed.
        key: String,
    },
    /// The `environment` entry for `key`.
    Environment {
        /// Environment key.
        key: String,
    },
    /// The `ports` entry at `index`.
    Port {
        /// Position in the `ports` list.
        index: usize,
    },
    /// The argv element of `command` at `index`.
    Command {
        /// Position in the split command.
        index: usize,
        /// Whether the program is a shell, so the text is a script the shell expands again.
        shell: bool,
    },
    /// The argv element of `healthcheck.test` at `index`.
    HealthcheckTest {
        /// Position in the split probe command.
        index: usize,
        /// Whether the program is a shell, so the text is a script the shell expands again.
        shell: bool,
    },
}

impl std::fmt::Display for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WorkingDir => f.write_str("working_dir"),
            Self::EnvFilePath { index } => write!(f, "env_file[{index}].path"),
            Self::EnvFileValue { index, path, key } => {
                write!(f, "key `{key}` in env_file[{index}] ({})", path.display())
            }
            Self::Environment { key } => write!(f, "environment.{key}"),
            Self::Port { index } => write!(f, "ports[{index}]"),
            Self::Command { index, .. } => write!(f, "command[{index}]"),
            Self::HealthcheckTest { index, .. } => write!(f, "healthcheck.test[{index}]"),
        }
    }
}

/// An `optional` env file that was not loaded, and why.
///
/// Reported as a note so an author whose variable "is in an env file" can see that the file
/// never took part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedEnvFile {
    /// Service the entry belongs to.
    pub service: ServiceID,
    /// Position of the entry in the `env_file` list.
    pub index: usize,
    /// Span of the entry's path in the config file.
    pub span: Span,
    /// Why the file was skipped.
    pub reason: SkipReason,
}

/// Why an `optional` env file was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The path references a variable that is not set.
    UnsetVariable {
        /// Name of the unset variable.
        variable: String,
    },
    /// Nothing exists at the resolved path, or it is not a regular file.
    NotAFile {
        /// Resolved path.
        path: PathBuf,
    },
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsetVariable { variable } => write!(f, "variable `{variable}` is not set"),
            Self::NotAFile { path } => write!(f, "no file at {}", path.display()),
        }
    }
}

impl std::fmt::Display for SkippedEnvFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            service,
            index,
            reason,
            ..
        } = self;
        write!(
            f,
            "optional env_file[{index}] of service `{service}` skipped: {reason}"
        )
    }
}

impl ToDiagnostics for SkippedEnvFile {
    fn to_diagnostics<F: Copy + PartialEq>(&self, file_id: F) -> Vec<Diagnostic<F>> {
        vec![
            Diagnostic::note()
                .with_message(self.to_string())
                .with_labels(vec![
                    Label::primary(file_id, self.span.clone()).with_message("skipped"),
                ]),
        ]
    }
}

/// Programs whose first argument after `-c` is a script the shell expands again at run time.
const SHELL_PROGRAMS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish", "cmd.exe", "cmd"];

fn is_shell_program(program: &str) -> bool {
    let name = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    SHELL_PROGRAMS
        .iter()
        .any(|shell| shell.eq_ignore_ascii_case(name))
}

impl Error {
    /// What the author can do about an interpolation failure at `site`.
    fn interpolation_help(site: &Site, source: &env::InterpolationError) -> String {
        let syntax = "supported references are `$VAR`, `${VAR}`, `${VAR:-default}`, and \
                      `${VAR-default}`; write `$$VAR` or `$${VAR}` to keep one literal";
        let env::InterpolationError::Unset { variable } = source else {
            return match site {
                Site::Command { .. } | Site::HealthcheckTest { .. } => format!(
                    "{syntax}; arguments are split before substitution, so quote a default \
                     that contains spaces: \"${{VAR:-two words}}\""
                ),
                _ => syntax.to_string(),
            };
        };
        match site {
            Site::WorkingDir => format!(
                "set `{variable}` in the environment micromux runs in, or write \
                 `${{{variable}:-<path>}}` to fall back to a default; paths only see that \
                 environment because they are resolved before any env_file is loaded"
            ),
            Site::EnvFilePath { .. } => format!(
                "set `{variable}` in the environment micromux runs in, write \
                 `${{{variable}:-<path>}}` to fall back to a default, or add `optional: true` to \
                 skip this file on machines where `{variable}` is unset"
            ),
            Site::EnvFileValue { .. } => format!(
                "define `{variable}` on an earlier line, in an earlier env_file, or in the \
                 environment micromux runs in; write `${{{variable}:-<default>}}`, or \
                 single-quote the value to keep it literal"
            ),
            Site::Environment { .. } => format!(
                "set `{variable}` in the environment micromux runs in, in an `env_file`, or \
                 above this entry under `environment:` (entries resolve top to bottom), or \
                 write `${{{variable}:-<default>}}` to fall back to a default"
            ),
            Site::Port { .. } => format!(
                "set `{variable}` in the environment micromux runs in, in an `env_file`, or \
                 under `environment:`, or write `${{{variable}:-<default>}}` to fall back to a \
                 default"
            ),
            // A shell script is where a name the shell defines itself is the likely cause, so
            // the literal escape comes first there and last everywhere else.
            Site::Command { shell: true, .. } | Site::HealthcheckTest { shell: true, .. } => {
                format!(
                    "write `$${variable}` to pass a literal `${variable}` through to the shell \
                     (for a variable the script defines itself), set `{variable}` in the \
                     environment micromux runs in, in an `env_file`, or under `environment:`, \
                     or write `${{{variable}:-<default>}}` to fall back to a default"
                )
            }
            Site::Command { shell: false, .. } | Site::HealthcheckTest { shell: false, .. } => {
                format!(
                    "set `{variable}` in the environment micromux runs in, in an `env_file`, or \
                     under `environment:`, write `${{{variable}:-<default>}}` to fall back to a \
                     default, or `$${variable}` to pass a literal `${variable}` through to the \
                     process"
                )
            }
        }
    }
}

impl ToDiagnostics for Error {
    fn to_diagnostics<F: Copy + PartialEq>(&self, file_id: F) -> Vec<Diagnostic<F>> {
        // The message already carries the cause, so the label under the caret names the site.
        // That matters for a string-form command, whose span covers the whole string rather
        // than the one argument that failed.
        let (span, label, notes) = match self {
            Self::Interpolation {
                site, span, source, ..
            } => (
                span,
                site.to_string(),
                vec![Self::interpolation_help(site, source)],
            ),
            Self::EnvFile { index, span, .. } => (span, format!("env_file[{index}].path"), vec![]),
            Self::InvalidPort {
                index, span, port, ..
            } => (
                span,
                format!("ports[{index}] resolves to `{port}`"),
                vec!["ports must resolve to a number between 0 and 65535".to_string()],
            ),
            Self::WorkingDirectory { span, .. } => (span, "working_dir".to_string(), vec![]),
        };
        vec![
            Diagnostic::error()
                .with_message(self.to_string())
                .with_labels(vec![
                    Label::primary(file_id, span.clone()).with_message(label),
                ])
                .with_notes(notes),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config,
        test_util::{service_config, spanned_string, unique_tmp_dir},
    };
    use color_eyre::eyre;
    use similar_asserts::assert_eq;
    use std::fs;
    use std::time::Duration;
    use yaml_spanned::Spanned;

    #[test]
    fn argv_flattens_program_and_args_and_defaults_working_dir() -> eyre::Result<()> {
        let dir = unique_tmp_dir("argv");
        std::fs::create_dir_all(&dir)?;
        let cfg = service_config("ui", ("task", &["tool:rag:ui:run:release"]));
        let service = Service::new("ui", &dir, cfg)?;
        assert_eq!(service.argv(), vec!["task", "tool:rag:ui:run:release"]);
        assert_eq!(service.working_dir_display(), None);
        Ok(())
    }

    #[test]
    fn env_file_missing_is_error() -> eyre::Result<()> {
        let dir = unique_tmp_dir("env-missing");
        std::fs::create_dir_all(&dir)?;
        let mut cfg = service_config("svc", ("sh", &["-c", "true"]));
        cfg.env_file = vec![config::EnvFile {
            path: spanned_string("./definitely-not-present.env"),
            optional: false,
        }];

        match Service::new("svc", &dir, cfg) {
            Ok(_) => Err(eyre::eyre!("expected error for missing env file")),
            Err(err) => {
                let msg = err.to_string();
                assert!(msg.contains("failed to read env file"), "{msg}");
                Ok(())
            }
        }
    }

    #[test]
    fn optional_missing_env_file_is_skipped() -> eyre::Result<()> {
        let dir = unique_tmp_dir("env-optional-missing");
        std::fs::create_dir_all(&dir)?;
        let mut cfg = service_config("svc", ("sh", &["-c", "true"]));
        cfg.env_file = vec![config::EnvFile {
            path: spanned_string("./definitely-not-present.env"),
            optional: true,
        }];

        let _service = Service::new("svc", &dir, cfg)?;
        Ok(())
    }

    /// [`Service::from_config`] with the skipped-file notes discarded.
    fn materialize(
        id: &str,
        config_dir: &Path,
        config: config::Service,
        supervisor_environment: &HashMap<String, String>,
    ) -> Result<Service, Error> {
        Service::from_config(
            id,
            config_dir,
            config,
            supervisor_environment,
            &mut Vec::new(),
        )
    }

    fn base_environment(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    /// The site and variable of the first interpolation failure, for compact assertions.
    fn unset_site(result: Result<Service, Error>) -> eyre::Result<(Site, String)> {
        match result {
            Err(Error::Interpolation {
                site,
                source: env::InterpolationError::Unset { variable },
                ..
            }) => Ok((site, variable)),
            Err(other) => Err(eyre::eyre!("unexpected error: {other}")),
            Ok(_) => Err(eyre::eyre!("expected an unset-variable error")),
        }
    }

    /// Paths resolve before any env file is loaded, so they only see the base environment and
    /// an unset variable there is an error for both `working_dir` and a required `env_file`.
    #[test]
    fn unset_variables_are_rejected_in_service_paths() -> eyre::Result<()> {
        let directory = unique_tmp_dir("unset-service-path");
        fs::create_dir_all(&directory)?;
        let base = base_environment(&[]);

        let mut working_dir = service_config("svc", ("sh", &["-c", "true"]));
        working_dir.working_dir = Some(spanned_string("${ROOT}/service"));
        assert_eq!(
            unset_site(materialize("svc", &directory, working_dir, &base))?,
            (Site::WorkingDir, "ROOT".to_string())
        );

        let mut env_file = service_config("svc", ("sh", &["-c", "true"]));
        env_file.env_file = vec![config::EnvFile {
            path: spanned_string("${ROOT}/service.env"),
            optional: false,
        }];
        assert_eq!(
            unset_site(materialize("svc", &directory, env_file, &base))?,
            (Site::EnvFilePath { index: 0 }, "ROOT".to_string())
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn working_directory_anchor_survives_path_replacement() -> eyre::Result<()> {
        let dir = unique_tmp_dir("working-dir-anchor");
        let working = dir.join("work");
        fs::create_dir_all(&working)?;
        fs::write(working.join("identity"), "original")?;
        let mut cfg = service_config("svc", ("sh", &["-c", "true"]));
        cfg.working_dir = Some(spanned_string(working.to_string_lossy().as_ref()));
        let service = Service::new("svc", &dir, cfg)?;

        fs::rename(&working, dir.join("old-work"))?;
        fs::create_dir_all(&working)?;
        fs::write(working.join("identity"), "replacement")?;

        let anchored = service
            .spawn_working_directory()?
            .ok_or_else(|| eyre::eyre!("working directory was not anchored"))?;
        assert_eq!(
            fs::read_to_string(anchored.as_path().join("identity"))?,
            "original"
        );
        let output = std::process::Command::new("sh")
            .args(["-c", "cat identity"])
            .current_dir(anchored.as_path())
            .output()?;
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout)?, "original");
        Ok(())
    }

    #[test]
    fn env_file_parse_error_is_error() -> eyre::Result<()> {
        let dir = unique_tmp_dir("env-parse-error");
        fs::create_dir_all(&dir)?;
        let env_path = dir.join("bad.env");
        fs::write(&env_path, "NOT_A_KV_LINE\n")?;

        let mut cfg = service_config("svc", ("sh", &["-c", "true"]));
        cfg.env_file = vec![config::EnvFile {
            path: spanned_string(env_path.to_string_lossy().as_ref()),
            optional: false,
        }];

        match Service::new("svc", &dir, cfg) {
            Ok(_) => Err(eyre::eyre!("expected error for invalid env file")),
            Err(err) => {
                let msg = err.to_string();
                assert!(msg.contains("failed to parse env file"), "{msg}");
                Ok(())
            }
        }
    }

    #[test]
    fn env_precedence_env_file_then_environment() -> eyre::Result<()> {
        let dir = unique_tmp_dir("env-precedence");
        fs::create_dir_all(&dir)?;
        let env_path = dir.join("svc.env");
        fs::write(&env_path, "FOO=from_file\n")?;

        let mut cfg = service_config("svc", ("sh", &["-c", "true"]));
        cfg.env_file = vec![config::EnvFile {
            path: spanned_string(env_path.to_string_lossy().as_ref()),
            optional: false,
        }];
        cfg.environment
            .insert(spanned_string("FOO"), spanned_string("from_config"));

        let svc = Service::new("svc", &dir, cfg)?;
        assert_eq!(
            svc.spec.environment.get("FOO").map(String::as_str),
            Some("from_config")
        );
        Ok(())
    }

    #[test]
    fn environment_and_ports_interpolate_using_merged_env() -> eyre::Result<()> {
        let dir = unique_tmp_dir("env-interpolate");
        fs::create_dir_all(&dir)?;
        let env_path = dir.join("svc.env");
        fs::write(&env_path, "BASE=10\n")?;

        let mut cfg = service_config("svc", ("sh", &["-c", "true"]));
        cfg.env_file = vec![config::EnvFile {
            path: spanned_string(env_path.to_string_lossy().as_ref()),
            optional: false,
        }];
        cfg.environment
            .insert(spanned_string("PORT"), spanned_string("${BASE}23"));
        cfg.ports.push(spanned_string("${PORT}"));

        let svc = Service::new("svc", &dir, cfg)?;
        assert_eq!(
            svc.spec.environment.get("PORT").map(String::as_str),
            Some("1023")
        );
        assert_eq!(svc.spec.ports, vec![1023]);
        Ok(())
    }

    #[test]
    fn config_service_materializes_one_complete_normalized_spec() -> eyre::Result<()> {
        let dir = unique_tmp_dir("normalized-spec");
        fs::create_dir_all(dir.join("work"))?;
        fs::write(dir.join("service.env"), "BASE=10\nFROM_FILE=yes\n")?;
        let mut cfg = service_config("worker", ("sh", &["-c", "echo ok"]));
        cfg.working_dir = Some(spanned_string("work"));
        cfg.env_file = vec![config::EnvFile {
            path: spanned_string("service.env"),
            optional: false,
        }];
        cfg.environment
            .insert(spanned_string("PORT"), spanned_string("${BASE}23"));
        cfg.environment
            .insert(spanned_string("FROM_FILE"), spanned_string("overridden"));
        cfg.depends_on = vec![config::Dependency {
            name: spanned_string("database"),
            condition: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: config::DependencyCondition::Healthy,
            }),
        }];
        cfg.healthcheck = Some(config::HealthCheck {
            test: (spanned_string("true"), Vec::new()),
            start_delay: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: Duration::from_millis(250),
            }),
            interval: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: Duration::from_secs(2),
            }),
            timeout: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: Duration::from_secs(1),
            }),
            retries: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: 0,
            }),
        });
        cfg.ports = vec![spanned_string("${PORT}")];
        cfg.restart_policy = RestartPolicy::Always;

        let service = Service::new("worker", &dir, cfg)?;

        assert_eq!(
            service.spec,
            ServiceSpec {
                name: Some("worker".to_string()),
                command: vec!["sh".to_string(), "-c".to_string(), "echo ok".to_string()],
                working_dir: Some(dir.join("work")),
                environment: indexmap::IndexMap::from([
                    ("BASE".to_string(), "10".to_string()),
                    ("FROM_FILE".to_string(), "overridden".to_string()),
                    ("PORT".to_string(), "1023".to_string()),
                ]),
                depends_on: vec![DependencySpec {
                    service: "database".to_string(),
                    condition: config::DependencyCondition::Healthy,
                }],
                healthcheck: Some(HealthcheckSpec {
                    test: vec!["true".to_string()],
                    start_delay: Some(Duration::from_millis(250)),
                    interval: Duration::from_secs(2),
                    timeout: Duration::from_secs(1),
                    retries: 1,
                }),
                ports: vec![1023],
                restart: RestartPolicy::Always,
                stop_grace_period: crate::spec::DEFAULT_STOP_GRACE_PERIOD,
                stop_signal: crate::spec::StopSignal::default(),
            }
        );
        Ok(())
    }

    #[test]
    fn env_file_path_is_relative_to_config_dir_and_expands_in_order() -> eyre::Result<()> {
        let dir = unique_tmp_dir("env-relative");
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join(".env"),
            "SPICEDB_PORT=50051\nDEMO_API_SPICEDB_ENDPOINT=\"http://0.0.0.0:${SPICEDB_PORT}\"\n",
        )?;

        let mut cfg = service_config("svc", ("sh", &["-c", "true"]));
        cfg.env_file = vec![config::EnvFile {
            path: spanned_string("./.env"),
            optional: false,
        }];

        let svc = Service::new("svc", &dir, cfg)?;
        assert_eq!(
            svc.spec
                .environment
                .get("DEMO_API_SPICEDB_ENDPOINT")
                .map(String::as_str),
            Some("http://0.0.0.0:50051")
        );
        Ok(())
    }

    /// `command` and `healthcheck.test` resolve against the same merged environment as
    /// `ports`, element by element, and `$$` reaches the process as a single `$`.
    #[test]
    fn command_and_healthcheck_interpolate_using_the_service_environment() -> eyre::Result<()> {
        let dir = unique_tmp_dir("command-interpolate");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("svc.env"), "BASE=10\n")?;

        let mut cfg = service_config(
            "svc",
            ("${SERVER}", &["--port", "${PORT}", "--name", "$$literal"]),
        );
        cfg.env_file = vec![config::EnvFile {
            path: spanned_string("svc.env"),
            optional: false,
        }];
        cfg.environment
            .insert(spanned_string("PORT"), spanned_string("${BASE}23"));
        cfg.healthcheck = Some(config::HealthCheck {
            test: (spanned_string("probe"), vec![spanned_string(":${PORT}")]),
            start_delay: None,
            interval: None,
            timeout: None,
            retries: None,
        });

        let base = base_environment(&[("SERVER", "server")]);
        let service = materialize("svc", &dir, cfg, &base)?;

        assert_eq!(
            service.spec.command,
            vec!["server", "--port", "1023", "--name", "$literal"]
        );
        assert_eq!(
            service.spec.healthcheck.map(|healthcheck| healthcheck.test),
            Some(vec!["probe".to_string(), ":1023".to_string()])
        );
        Ok(())
    }

    /// Substitution runs after argv splitting, so a quoted `"${VAR}"` in a string command stays
    /// one argument even when the value contains whitespace, and shell-lowered forms still
    /// receive the substituted text.
    #[test]
    fn string_commands_substitute_per_argument_after_splitting() -> eyre::Result<()> {
        let dir = unique_tmp_dir("command-split");
        fs::create_dir_all(&dir)?;
        let yaml = indoc::indoc! {r#"
            version: 1
            services:
              app:
                command: 'serve --token "${TOKEN}" --flag'
                environment:
                  TOKEN: "two words"
                  PORT: "8080"
                healthcheck:
                  test: ["CMD-SHELL", "curl", "-fsS", "http://localhost:${PORT}/"]
        "#};
        let mut diagnostics = Vec::new();
        let parsed = config::from_str(yaml, &dir, 0usize, None, &mut diagnostics)?;
        let (name, cfg) = parsed
            .config
            .services
            .into_iter()
            .next()
            .ok_or_else(|| eyre::eyre!("missing service"))?;

        let service = materialize(name.as_ref(), &dir, cfg, &base_environment(&[]))?;

        assert_eq!(
            service.spec.command,
            vec!["serve", "--token", "two words", "--flag"]
        );
        let test = service
            .spec
            .healthcheck
            .map(|healthcheck| healthcheck.test)
            .ok_or_else(|| eyre::eyre!("missing healthcheck"))?;
        assert_eq!(
            test.last().map(String::as_str),
            Some("curl -fsS http://localhost:8080/")
        );
        Ok(())
    }

    /// Shell syntax that is not a micromux reference reaches the process unchanged, so a
    /// deliberate `sh -c` payload keeps its positional parameters and command substitutions.
    #[test]
    fn non_reference_dollars_pass_through_to_argv() -> eyre::Result<()> {
        let dir = unique_tmp_dir("dollar-passthrough");
        fs::create_dir_all(&dir)?;
        let cfg = service_config("svc", ("sh", &["-c", "echo $1 $? $$ $(pwd) $$HOME"]));

        let service = materialize("svc", &dir, cfg, &base_environment(&[]))?;

        assert_eq!(
            service.spec.command,
            vec!["sh", "-c", "echo $1 $? $$ $(pwd) $HOME"]
        );
        Ok(())
    }

    /// Every interpolating site reports an unset variable as an error that names the site,
    /// instead of substituting an empty string.
    #[test]
    fn unset_variables_are_rejected_at_every_site() -> eyre::Result<()> {
        let dir = unique_tmp_dir("unset-sites");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("bad.env"), "URL=http://${MISSING_IN_FILE}/\n")?;
        let base = base_environment(&[]);
        let plain = || service_config("svc", ("true", &[]));

        let mut environment = plain();
        environment
            .environment
            .insert(spanned_string("KEY"), spanned_string("${MISSING_ENV}"));

        let mut port = plain();
        port.ports = vec![spanned_string("${MISSING_PORT}")];

        let command = service_config("svc", ("run", &["--flag", "${MISSING_ARG}"]));

        let mut healthcheck = plain();
        healthcheck.healthcheck = Some(config::HealthCheck {
            test: (
                spanned_string("probe"),
                vec![spanned_string("${MISSING_PROBE}")],
            ),
            start_delay: None,
            interval: None,
            timeout: None,
            retries: None,
        });

        let mut env_file = plain();
        env_file.env_file = vec![config::EnvFile {
            path: spanned_string("bad.env"),
            optional: false,
        }];

        let cases = [
            (
                environment,
                Site::Environment {
                    key: "KEY".to_string(),
                },
                "MISSING_ENV",
            ),
            (port, Site::Port { index: 0 }, "MISSING_PORT"),
            (
                command,
                Site::Command {
                    index: 2,
                    shell: false,
                },
                "MISSING_ARG",
            ),
            (
                healthcheck,
                Site::HealthcheckTest {
                    index: 1,
                    shell: false,
                },
                "MISSING_PROBE",
            ),
            (
                env_file,
                Site::EnvFileValue {
                    index: 0,
                    path: dir.join("bad.env").into_boxed_path(),
                    key: "URL".to_string(),
                },
                "MISSING_IN_FILE",
            ),
        ];
        for (cfg, expected_site, expected_variable) in cases {
            let result = materialize("svc", &dir, cfg, &base);
            assert_eq!(
                unset_site(result)?,
                (expected_site, expected_variable.to_string())
            );
        }
        Ok(())
    }

    #[test]
    fn invalid_port_error_names_the_site_and_resolved_value() -> eyre::Result<()> {
        let dir = unique_tmp_dir("invalid-port");
        fs::create_dir_all(&dir)?;
        let mut cfg = service_config("svc", ("true", &[]));
        cfg.ports = vec![spanned_string("80"), spanned_string("${PORT}")];

        let result = materialize("svc", &dir, cfg, &base_environment(&[("PORT", "http")]));

        match result {
            Err(Error::InvalidPort { index, port, .. }) => {
                assert_eq!(index, 1);
                assert_eq!(port, "http");
                Ok(())
            }
            other => Err(eyre::eyre!("expected an invalid port error, got {other:?}")),
        }
    }

    /// A machine-level env file named by a variable loads where the variable is set, layers
    /// between the committed `.env` and the local override, and is skipped elsewhere.
    #[test]
    fn optional_env_file_named_by_a_variable_is_layered_or_skipped() -> eyre::Result<()> {
        let dir = unique_tmp_dir("optional-shared-env");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join(".env"), "A=repo\nB=repo\nC=repo\n")?;
        fs::write(dir.join("shared.env"), "B=shared\nC=shared\n")?;
        fs::write(dir.join(".env.local"), "C=local\n")?;
        let cfg = || {
            let mut cfg = service_config("svc", ("true", &[]));
            cfg.env_file = vec![
                config::EnvFile {
                    path: spanned_string(".env"),
                    optional: false,
                },
                config::EnvFile {
                    path: spanned_string("${SHARED_ENV_FILE}"),
                    optional: true,
                },
                config::EnvFile {
                    path: spanned_string(".env.local"),
                    optional: true,
                },
            ];
            cfg
        };
        let values = |service: Service| {
            ["A", "B", "C"].map(|key| {
                service
                    .spec
                    .environment
                    .get(key)
                    .cloned()
                    .unwrap_or_default()
            })
        };

        // Opted-in machine: the shared file overrides `.env` and loses to `.env.local`.
        let shared = dir.join("shared.env").to_string_lossy().into_owned();
        let opted_in = materialize(
            "svc",
            &dir,
            cfg(),
            &base_environment(&[("SHARED_ENV_FILE", &shared)]),
        )?;
        assert_eq!(values(opted_in), ["repo", "shared", "local"]);

        // Machine without the variable: the entry is skipped and the rest still loads.
        let skipped = materialize("svc", &dir, cfg(), &base_environment(&[]))?;
        assert_eq!(values(skipped), ["repo", "repo", "local"]);

        // Variable set but pointing nowhere: also skipped, because the file cannot be located.
        let dangling = materialize(
            "svc",
            &dir,
            cfg(),
            &base_environment(&[("SHARED_ENV_FILE", "/definitely/not/here.env")]),
        )?;
        assert_eq!(values(dangling), ["repo", "repo", "local"]);
        Ok(())
    }

    /// Optional only covers a file that cannot be located: a malformed reference in its path
    /// and a present-but-invalid file remain errors.
    #[test]
    fn optional_env_file_still_reports_syntax_and_parse_errors() -> eyre::Result<()> {
        let dir = unique_tmp_dir("optional-env-errors");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("broken.env"), "NOT_A_KV_LINE\n")?;

        let mut unterminated = service_config("svc", ("true", &[]));
        unterminated.env_file = vec![config::EnvFile {
            path: spanned_string("${SHARED"),
            optional: true,
        }];
        assert!(matches!(
            materialize("svc", &dir, unterminated, &base_environment(&[])),
            Err(Error::Interpolation {
                site: Site::EnvFilePath { index: 0 },
                source: env::InterpolationError::Unterminated,
                ..
            })
        ));

        let mut broken = service_config("svc", ("true", &[]));
        broken.env_file = vec![config::EnvFile {
            path: spanned_string("broken.env"),
            optional: true,
        }];
        assert!(matches!(
            materialize("svc", &dir, broken, &base_environment(&[])),
            Err(Error::EnvFile { index: 0, .. })
        ));
        Ok(())
    }

    /// Every skipped optional file is reported with its reason, whether or not the service
    /// materializes, so a later "variable is not set" can be traced to the file that never
    /// loaded.
    #[test]
    fn skipped_optional_env_files_are_reported() -> eyre::Result<()> {
        let dir = unique_tmp_dir("skipped-env-notes");
        fs::create_dir_all(&dir)?;
        let mut cfg = service_config("svc", ("true", &[]));
        cfg.env_file = vec![
            config::EnvFile {
                path: spanned_string("${SHARED_ENV_FILE}"),
                optional: true,
            },
            config::EnvFile {
                path: spanned_string("missing.env"),
                optional: true,
            },
        ];
        cfg.environment
            .insert(spanned_string("NEEDS"), spanned_string("${FROM_SHARED}"));

        let mut skipped = Vec::new();
        let result = Service::from_config("svc", &dir, cfg, &base_environment(&[]), &mut skipped);

        assert_eq!(
            unset_site(result)?,
            (
                Site::Environment {
                    key: "NEEDS".to_string()
                },
                "FROM_SHARED".to_string()
            )
        );
        let reasons = skipped
            .iter()
            .map(|entry| (entry.index, entry.reason.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            reasons,
            vec![
                (
                    0,
                    SkipReason::UnsetVariable {
                        variable: "SHARED_ENV_FILE".to_string()
                    }
                ),
                (
                    1,
                    SkipReason::NotAFile {
                        path: dir.join("missing.env")
                    }
                ),
            ]
        );
        Ok(())
    }

    #[test]
    fn env_file_path_defaults_select_a_fallback_file() -> eyre::Result<()> {
        let dir = unique_tmp_dir("env-path-default");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("fallback.env"), "FROM=fallback\n")?;
        let mut cfg = service_config("svc", ("true", &[]));
        cfg.env_file = vec![config::EnvFile {
            path: spanned_string("${SHARED_ENV_FILE:-./fallback.env}"),
            optional: false,
        }];

        let service = materialize("svc", &dir, cfg, &base_environment(&[]))?;

        assert_eq!(
            service.spec.environment.get("FROM").map(String::as_str),
            Some("fallback")
        );
        Ok(())
    }
}

#[derive(
    Debug,
    Default,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
/// How a service should be restarted after it exits.
pub enum RestartPolicy {
    /// Always restart the service when it exits.
    Always,
    /// Restart the service unless it was explicitly stopped.
    UnlessStopped,
    /// Never restart the service automatically.
    #[default]
    Never,
    /// Restart only after a non-zero exit.
    OnFailure {
        /// Maximum number of automatic restarts after a non-zero exit.
        ///
        /// `None` means unlimited (matching Docker Compose `on-failure` without a count).
        max_attempts: Option<usize>,
    },
}

impl std::fmt::Display for RestartPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Always => write!(f, "Always"),
            Self::UnlessStopped => write!(f, "UnlessStopped"),
            Self::Never => write!(f, "Never"),
            Self::OnFailure { max_attempts } => f
                .debug_struct("OnFailure")
                .field("max_attempts", max_attempts)
                .finish(),
        }
    }
}

/// Determines how a service enters a new session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StartupMode {
    /// Start automatically once dependencies are ready.
    #[default]
    Enabled,
    /// Wait for an explicit enable request.
    Disabled,
}

#[derive(Debug, Clone)]
pub struct Service {
    pub id: ServiceID,
    pub spec: ServiceSpec,
    pub origin: ServiceOrigin,
    pub startup_mode: StartupMode,
    pub enable_color: bool,
    pub log_retention: LogRetention,
    // Keeps each spawn tied to the directory that passed validation even if its path is replaced.
    #[cfg(unix)]
    working_directory: Option<Arc<std::fs::File>>,
}

/// A spawn path that retains its validated directory identity where the platform supports it.
///
/// Holding a value keeps the anchored directory descriptor open, so a descriptor-backed path
/// such as `/proc/self/fd/N` stays spawnable even after the owning [`Service`] drops or
/// replaces its anchor (for example when config reconciliation replaces a live definition).
#[derive(Debug, Clone)]
pub(crate) struct SpawnWorkingDirectory {
    path: PathBuf,
    #[cfg(unix)]
    #[expect(
        dead_code,
        reason = "held for RAII: the open directory keeps the descriptor-backed spawn path valid"
    )]
    directory: Arc<std::fs::File>,
}

impl SpawnWorkingDirectory {
    pub(crate) fn as_path(&self) -> &Path {
        &self.path
    }
}

impl Service {
    pub(crate) fn dynamic(
        id: ServiceID,
        spec: ServiceSpec,
        origin: ServiceOrigin,
        log_retention: LogRetention,
    ) -> Result<Self, WorkingDirectoryError> {
        #[cfg(unix)]
        let working_directory = spec
            .working_dir
            .as_deref()
            .map(open_working_directory)
            .transpose()?;
        #[cfg(not(unix))]
        spec.working_dir
            .as_deref()
            .map(validate_working_directory)
            .transpose()?;
        Ok(Self {
            id,
            spec,
            origin,
            startup_mode: StartupMode::Enabled,
            enable_color: true,
            log_retention,
            #[cfg(unix)]
            working_directory,
        })
    }

    /// Materialize a configured service against the environment micromux itself runs in.
    ///
    /// Config loads go through [`Service::from_config`] so that one environment snapshot serves
    /// every service; this convenience reads the process environment for a single service and
    /// discards the skipped-file notes.
    ///
    /// # Errors
    ///
    /// Same as [`Service::from_config`].
    #[cfg(test)]
    pub fn new(
        id: impl Into<ServiceID>,
        config_dir: &Path,
        config: config::Service,
    ) -> Result<Self, Error> {
        Self::from_config(
            id,
            config_dir,
            config,
            &std::env::vars().collect(),
            &mut Vec::new(),
        )
    }

    /// Materialize a configured service, resolving variable references against
    /// `supervisor_environment`, the environment micromux itself runs in.
    ///
    /// Values resolve in layers so that no field can depend on something not yet known:
    ///
    /// - `working_dir` and every `env_file` path see only `supervisor_environment`, because the
    ///   files have to be located before anything can be loaded from them.
    /// - Each env file sees `supervisor_environment`, the files before it, and its own earlier
    ///   lines.
    /// - `environment` entries additionally see every loaded env file and their earlier siblings.
    /// - `ports`, `command`, and `healthcheck.test` see the complete environment the service
    ///   process receives.
    ///
    /// Every `optional` env file that was not loaded is appended to `skipped_env_files`, on
    /// success and on failure alike, because a skipped file often explains a later unset
    /// variable.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Interpolation`] when a configured value references a variable that
    /// cannot be substituted, [`Error::EnvFile`] when an environment file cannot be loaded,
    /// [`Error::InvalidPort`] when a port does not resolve to a number, and
    /// [`Error::WorkingDirectory`] when the working directory is not usable.
    pub(crate) fn from_config(
        id: impl Into<ServiceID>,
        config_dir: &Path,
        config: config::Service,
        supervisor_environment: &HashMap<String, String>,
        skipped_env_files: &mut Vec<SkippedEnvFile>,
    ) -> Result<Self, Error> {
        let id: ServiceID = id.into();
        let resolver = Resolver {
            service: &id,
            config_dir,
        };

        // Paths: supervisor environment only
        let resolved_working_dir = config
            .working_dir
            .as_ref()
            .map(|dir| resolver.working_directory(dir, supervisor_environment))
            .transpose()?;
        #[cfg(unix)]
        let working_directory = resolved_working_dir
            .as_ref()
            .map(|dir| Arc::clone(&dir.anchor));
        let working_dir = resolved_working_dir.map(|dir| dir.path);

        // Env files, then inline entries, each layered over everything resolved before it
        let mut environment: indexmap::IndexMap<String, String> = resolver
            .load_env_files(&config.env_file, supervisor_environment, skipped_env_files)?
            .into_iter()
            .collect();
        let mut service_environment = supervisor_environment.clone();
        service_environment.extend(environment.clone());
        for (key, value) in &config.environment {
            let site = Site::Environment {
                key: key.as_ref().clone(),
            };
            let expanded = resolver.interpolate(site, value, &service_environment)?;
            environment.insert(key.as_ref().clone(), expanded.clone());
            service_environment.insert(key.as_ref().clone(), expanded);
        }

        // Everything else: the environment the process receives
        let ports = config
            .ports
            .iter()
            .enumerate()
            .map(|(index, port)| resolver.port(index, port, &service_environment))
            .collect::<Result<Vec<_>, _>>()?;
        let (program, args) = &config.command;
        let shell = is_shell_program(program.as_ref());
        let command = resolver.argv(
            |index| Site::Command { index, shell },
            std::iter::once(program).chain(args),
            &service_environment,
        )?;
        let healthcheck = config
            .healthcheck
            .map(|healthcheck| {
                let (program, args) = &healthcheck.test;
                let shell = is_shell_program(program.as_ref());
                let test = resolver.argv(
                    |index| Site::HealthcheckTest { index, shell },
                    std::iter::once(program).chain(args),
                    &service_environment,
                )?;
                Ok::<_, Error>(HealthcheckSpec::from_config(healthcheck, test))
            })
            .transpose()?;

        let depends_on = config
            .depends_on
            .into_iter()
            .map(|dependency| DependencySpec {
                service: dependency.name.into_inner(),
                condition: dependency
                    .condition
                    .map(yaml_spanned::Spanned::into_inner)
                    .unwrap_or_default(),
            })
            .collect();

        Ok(Self {
            id,
            spec: ServiceSpec {
                name: Some(config.name.into_inner()),
                command,
                working_dir,
                environment,
                depends_on,
                healthcheck,
                ports,
                restart: config.restart_policy,
                stop_grace_period: config.stop_grace_period.into_inner(),
                stop_signal: config.stop_signal.into_inner(),
            },
            origin: ServiceOrigin::Configured,
            startup_mode: config.startup_mode,
            enable_color: config.color.as_deref().copied().unwrap_or(true),
            log_retention: config.log_retention,
            #[cfg(unix)]
            working_directory,
        })
    }

    /// The resolved program and arguments this service runs, as a single argv vector.
    #[must_use]
    pub fn argv(&self) -> Vec<String> {
        self.spec.command.clone()
    }

    /// The service's overridden working directory as a display string, or `None` when it inherits
    /// the session's working directory (the directory micromux was launched in).
    #[must_use]
    pub fn working_dir_display(&self) -> Option<String> {
        self.spec.working_dir_display()
    }

    pub fn display_name(&self) -> &str {
        self.spec.name.as_deref().unwrap_or(&self.id)
    }

    #[cfg_attr(
        not(unix),
        expect(
            clippy::unnecessary_wraps,
            reason = "only the unix anchor resolution can fail; the fallible signature is shared across platforms"
        )
    )]
    pub(crate) fn spawn_working_directory(
        &self,
    ) -> Result<Option<SpawnWorkingDirectory>, WorkingDirectoryError> {
        #[cfg(unix)]
        {
            self.working_directory
                .as_ref()
                .map(|directory| {
                    let path = {
                        #[cfg(target_vendor = "apple")]
                        {
                            use std::ffi::OsString;
                            use std::os::unix::ffi::OsStringExt as _;

                            // macOS exposes `/dev/fd/N` as the directory itself but does not allow
                            // path traversal below it, so recover the anchored vnode's current path.
                            let path =
                                rustix::fs::getpath(directory.as_ref()).map_err(|source| {
                                    WorkingDirectoryError {
                                        path: self
                                            .spec
                                            .working_dir
                                            .clone()
                                            .unwrap_or_else(|| Path::new(".").to_path_buf()),
                                        source: source.into(),
                                    }
                                })?;
                            PathBuf::from(OsString::from_vec(path.into_bytes()))
                        }

                        #[cfg(not(target_vendor = "apple"))]
                        {
                            use std::os::fd::AsRawFd as _;

                            let fd = directory.as_raw_fd().to_string();
                            #[cfg(target_os = "linux")]
                            let base = "/proc/self/fd";
                            #[cfg(not(target_os = "linux"))]
                            let base = "/dev/fd";
                            Path::new(base).join(fd)
                        }
                    };
                    Ok(SpawnWorkingDirectory {
                        path,
                        directory: Arc::clone(directory),
                    })
                })
                .transpose()
        }

        #[cfg(not(unix))]
        {
            Ok(self
                .spec
                .working_dir
                .clone()
                .map(|path| SpawnWorkingDirectory { path }))
        }
    }

    pub(crate) fn replace_spec(&mut self, spec: ServiceSpec) -> Result<(), WorkingDirectoryError> {
        #[cfg(unix)]
        {
            self.working_directory = spec
                .working_dir
                .as_deref()
                .map(open_working_directory)
                .transpose()?;
        }
        #[cfg(not(unix))]
        spec.working_dir
            .as_deref()
            .map(validate_working_directory)
            .transpose()?;
        self.spec = spec;
        Ok(())
    }
}

/// A configured working directory that resolved and exists.
struct WorkingDirectory {
    path: PathBuf,
    #[cfg(unix)]
    anchor: Arc<std::fs::File>,
}

/// Labels every failure of one service's configured values with its site and span.
struct Resolver<'a> {
    service: &'a ServiceID,
    config_dir: &'a Path,
}

impl Resolver<'_> {
    fn interpolation_error(
        &self,
        site: Site,
        value: &yaml_spanned::Spanned<String>,
        source: env::InterpolationError,
    ) -> Error {
        Error::Interpolation {
            service: self.service.clone(),
            site,
            span: value.span.into(),
            source,
        }
    }

    fn resolve_path(
        &self,
        site: Site,
        value: &yaml_spanned::Spanned<String>,
        environment: &HashMap<String, String>,
    ) -> Result<PathBuf, Error> {
        env::resolve_path(self.config_dir, value.as_ref(), environment)
            .map_err(|source| self.interpolation_error(site, value, source))
    }

    fn working_directory(
        &self,
        value: &yaml_spanned::Spanned<String>,
        environment: &HashMap<String, String>,
    ) -> Result<WorkingDirectory, Error> {
        let path = self.resolve_path(Site::WorkingDir, value, environment)?;
        let error = |source| Error::WorkingDirectory {
            service: self.service.clone(),
            span: value.span.into(),
            source,
        };
        #[cfg(unix)]
        let anchor = open_working_directory(&path).map_err(error)?;
        #[cfg(not(unix))]
        validate_working_directory(&path).map_err(error)?;
        Ok(WorkingDirectory {
            path,
            #[cfg(unix)]
            anchor,
        })
    }

    fn interpolate(
        &self,
        site: Site,
        value: &yaml_spanned::Spanned<String>,
        environment: &HashMap<String, String>,
    ) -> Result<String, Error> {
        env::interpolate(value.as_ref(), environment)
            .map_err(|source| self.interpolation_error(site, value, source))
    }

    fn port(
        &self,
        index: usize,
        value: &yaml_spanned::Spanned<String>,
        environment: &HashMap<String, String>,
    ) -> Result<u16, Error> {
        let port = self.interpolate(Site::Port { index }, value, environment)?;
        port.parse::<u16>().map_err(|source| Error::InvalidPort {
            service: self.service.clone(),
            index,
            span: value.span.into(),
            port,
            source,
        })
    }

    fn argv<'v>(
        &self,
        site: impl Fn(usize) -> Site,
        parts: impl Iterator<Item = &'v yaml_spanned::Spanned<String>>,
        environment: &HashMap<String, String>,
    ) -> Result<Vec<String>, Error> {
        parts
            .enumerate()
            .map(|(index, part)| self.interpolate(site(index), part, environment))
            .collect()
    }

    /// Load the configured env files in order, each expanded over `supervisor_environment` and
    /// the files before it.
    ///
    /// An `optional` entry whose path references an unset variable, or that does not point at
    /// a file, is recorded in `skipped` instead of loaded.
    /// A present optional file participates fully, including parse errors.
    fn load_env_files(
        &self,
        entries: &[config::EnvFile],
        supervisor_environment: &HashMap<String, String>,
        skipped: &mut Vec<SkippedEnvFile>,
    ) -> Result<env::EnvMap, Error> {
        let mut loaded = env::EnvMap::new();
        let mut current = supervisor_environment.clone();
        for (index, entry) in entries.iter().enumerate() {
            let mut skip = |reason: SkipReason| {
                tracing::info!(
                    service_id = %self.service,
                    index,
                    %reason,
                    "skipping optional env file"
                );
                skipped.push(SkippedEnvFile {
                    service: self.service.clone(),
                    index,
                    span: entry.path.span.into(),
                    reason,
                });
            };
            let path = match env::resolve_path(
                self.config_dir,
                entry.path.as_ref(),
                supervisor_environment,
            ) {
                Ok(path) => path,
                Err(env::InterpolationError::Unset { variable }) if entry.optional => {
                    skip(SkipReason::UnsetVariable { variable });
                    continue;
                }
                Err(source) => {
                    return Err(self.interpolation_error(
                        Site::EnvFilePath { index },
                        &entry.path,
                        source,
                    ));
                }
            };
            if entry.optional && !path.is_file() {
                skip(SkipReason::NotAFile { path });
                continue;
            }
            let parsed =
                env::load_env_files_sync(std::slice::from_ref(&path)).map_err(|source| {
                    Error::EnvFile {
                        service: self.service.clone(),
                        index,
                        span: entry.path.span.into(),
                        source,
                    }
                })?;
            let expanded = env::expand_env_values(&parsed, &current).map_err(|err| {
                let env::ValueError { key, source } = err;
                let site = Site::EnvFileValue {
                    index,
                    path: path.clone().into_boxed_path(),
                    key,
                };
                self.interpolation_error(site, &entry.path, source)
            })?;
            current.extend(expanded.clone());
            loaded.extend(expanded);
        }
        Ok(loaded)
    }
}

#[cfg(unix)]
fn open_working_directory(path: &Path) -> Result<Arc<std::fs::File>, WorkingDirectoryError> {
    let error = |source| WorkingDirectoryError {
        path: path.to_path_buf(),
        source,
    };
    let directory = std::fs::File::open(path).map_err(error)?;
    if !directory.metadata().map_err(error)?.is_dir() {
        return Err(error(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "path is not a directory",
        )));
    }
    Ok(Arc::new(directory))
}

#[cfg(not(unix))]
fn validate_working_directory(path: &Path) -> Result<(), WorkingDirectoryError> {
    let error = |source| WorkingDirectoryError {
        path: path.to_path_buf(),
        source,
    };
    if !std::fs::metadata(path).map_err(error)?.is_dir() {
        return Err(error(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "path is not a directory",
        )));
    }
    Ok(())
}
