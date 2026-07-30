use crate::{
    config::{self},
    env,
    model::LogRetention,
    scheduler::ServiceID,
    spec::{DependencySpec, HealthcheckSpec, ServiceOrigin, ServiceSpec},
};
use std::path::Path;

/// Errors from materializing a runnable service definition.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Environment files or configured paths could not be resolved.
    #[error(transparent)]
    Environment(#[from] env::Error),
    /// An advertised port is not a valid TCP/UDP port number.
    #[error("invalid port `{port}`: {source}")]
    InvalidPort {
        /// Expanded port value.
        port: String,
        /// Underlying integer parse error.
        #[source]
        source: std::num::ParseIntError,
    },
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
            "SPICEDB_PORT=50051\nAIRTYPE_API_SPICEDB_ENDPOINT=\"http://0.0.0.0:${SPICEDB_PORT}\"\n",
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
                .get("AIRTYPE_API_SPICEDB_ENDPOINT")
                .map(String::as_str),
            Some("http://0.0.0.0:50051")
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
}

impl Service {
    pub(crate) fn dynamic(
        id: ServiceID,
        spec: ServiceSpec,
        origin: ServiceOrigin,
        log_retention: LogRetention,
    ) -> Self {
        Self {
            id,
            spec,
            origin,
            startup_mode: StartupMode::Enabled,
            enable_color: true,
            log_retention,
        }
    }

    pub fn new(
        id: impl Into<ServiceID>,
        config_dir: &Path,
        config: config::Service,
    ) -> Result<Self, Error> {
        let (prog, args) = config.command;
        let id: ServiceID = id.into();

        let working_dir = config
            .working_dir
            .as_ref()
            .map(|dir| env::resolve_path(config_dir, dir.as_ref()))
            .transpose()?;

        let mut env_files = Vec::new();
        for env_file in &config.env_file {
            let path = env::resolve_path(config_dir, env_file.path.as_ref())?;
            // A missing optional file is skipped; a present one still
            // participates fully, including parse errors.
            if env_file.optional && !path.exists() {
                continue;
            }
            env_files.push(path);
        }

        let base_env: std::collections::HashMap<String, String> = std::env::vars().collect();
        let mut missing_env = Vec::new();
        let env_file_env = env::load_env_files_sync(&env_files)?;
        let env_file_env =
            env::expand_env_values_tracking(&env_file_env, &base_env, &mut missing_env);

        let mut base_with_env_file = base_env.clone();
        for (k, v) in env_file_env.iter() {
            base_with_env_file.insert(k.clone(), v.clone());
        }

        let mut config_env_map = env::EnvMap::new();
        for (k, v) in &config.environment {
            config_env_map.insert(k.as_ref().clone(), v.as_ref().clone());
        }
        let config_env_map =
            env::expand_env_values_tracking(&config_env_map, &base_with_env_file, &mut missing_env);

        let mut full_env = base_with_env_file.clone();
        for (k, v) in config_env_map.iter() {
            full_env.insert(k.clone(), v.clone());
        }

        let advertised_ports = config
            .ports
            .iter()
            .map(|port| {
                let expanded =
                    env::interpolate_str_tracking(port.as_ref(), &full_env, &mut missing_env);
                expanded
                    .parse::<u16>()
                    .map_err(|source| Error::InvalidPort {
                        port: expanded,
                        source,
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;

        missing_env.sort_unstable();
        missing_env.dedup();
        if !missing_env.is_empty() {
            tracing::warn!(
                service_id = %id,
                missing = ?missing_env,
                "environment interpolation referenced unset variables"
            );
        }

        let mut environment = indexmap::IndexMap::new();
        for (k, v) in env_file_env.iter() {
            environment.insert(k.clone(), v.clone());
        }
        for (k, v) in config_env_map.iter() {
            environment.insert(k.clone(), v.clone());
        }

        let healthcheck = config.healthcheck.map(HealthcheckSpec::from);
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
        let mut command = vec![prog.into_inner()];
        command.extend(args.into_iter().map(yaml_spanned::Spanned::into_inner));

        Ok(Self {
            id,
            spec: ServiceSpec {
                name: Some(config.name.into_inner()),
                command,
                working_dir,
                environment,
                depends_on,
                healthcheck,
                ports: advertised_ports,
                restart: config.restart_policy,
                stop_grace_period: config.stop_grace_period.into_inner(),
            },
            origin: ServiceOrigin::Configured,
            startup_mode: config.startup_mode,
            enable_color: config.color.as_deref().copied().unwrap_or(true),
            log_retention: config.log_retention,
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
}
