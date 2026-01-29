use color_eyre::eyre;
use std::path::{Path, PathBuf};
use yaml_spanned::Spanned;

use crate::{
    config::{self},
    env,
    scheduler::ServiceID,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use indexmap::IndexMap;
    use std::fs;

    use std::time::{SystemTime, UNIX_EPOCH};
    use yaml_spanned::Spanned;

    fn spanned_string(value: &str) -> Spanned<String> {
        Spanned {
            span: yaml_spanned::spanned::Span::default(),
            inner: value.to_string(),
        }
    }

    fn unique_tmp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("micromux-{prefix}-{nanos}"))
    }

    fn service_config(name: &str, command: (&str, &[&str])) -> config::Service {
        config::Service {
            name: spanned_string(name),
            command: (
                spanned_string(command.0),
                command
                    .1
                    .iter()
                    .map(|v| spanned_string(v))
                    .collect::<Vec<_>>(),
            ),
            working_dir: None,
            env_file: vec![],
            environment: IndexMap::new(),
            depends_on: vec![],
            healthcheck: None,
            ports: vec![],
            restart: None,
            color: None,
        }
    }

    #[test]
    fn env_file_missing_is_error() -> eyre::Result<()> {
        let dir = unique_tmp_dir("env-missing");
        std::fs::create_dir_all(&dir)?;
        let mut cfg = service_config("svc", ("sh", &["-c", "true"]));
        cfg.env_file = vec![config::EnvFile {
            path: spanned_string("./definitely-not-present.env"),
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
    fn env_file_parse_error_is_error() -> eyre::Result<()> {
        let dir = unique_tmp_dir("env-parse-error");
        fs::create_dir_all(&dir)?;
        let env_path = dir.join("bad.env");
        fs::write(&env_path, "NOT_A_KV_LINE\n")?;

        let mut cfg = service_config("svc", ("sh", &["-c", "true"]));
        cfg.env_file = vec![config::EnvFile {
            path: spanned_string(env_path.to_string_lossy().as_ref()),
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
        }];
        cfg.environment
            .insert(spanned_string("FOO"), spanned_string("from_config"));

        let svc = Service::new("svc", &dir, cfg)?;
        assert_eq!(
            svc.environment.get("FOO").map(String::as_str),
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
        }];
        cfg.environment
            .insert(spanned_string("PORT"), spanned_string("${BASE}23"));
        cfg.ports.push(spanned_string("${PORT}"));

        let svc = Service::new("svc", &dir, cfg)?;
        assert_eq!(
            svc.environment.get("PORT").map(String::as_str),
            Some("1023")
        );
        assert_eq!(svc.open_ports, vec![1023]);
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
        }];

        let svc = Service::new("svc", &dir, cfg)?;
        assert_eq!(
            svc.environment
                .get("AIRTYPE_API_SPICEDB_ENDPOINT")
                .map(String::as_str),
            Some("http://0.0.0.0:50051")
        );
        Ok(())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RestartPolicy {
    Always,
    UnlessStopped,
    #[default]
    Never,
    OnFailure {
        remaining_attempts: usize,
    },
}

impl std::fmt::Display for RestartPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Always => write!(f, "Always"),
            Self::UnlessStopped => write!(f, "UnlessStopped"),
            Self::Never => write!(f, "Never"),
            Self::OnFailure { remaining_attempts } => f
                .debug_struct("OnFailure")
                .field("remaining", remaining_attempts)
                .finish(),
        }
    }
}

#[derive(Debug)]
pub struct Service {
    pub id: ServiceID,
    pub name: Spanned<String>,
    pub command: (String, Vec<String>),
    pub working_dir: Option<PathBuf>,
    pub restart_policy: RestartPolicy,
    pub depends_on: Vec<config::Dependency>,
    pub environment: indexmap::IndexMap<String, String>,
    pub health_check: Option<config::HealthCheck>,
    pub open_ports: Vec<u16>,
    pub enable_color: bool,
}

impl Service {
    pub fn new(
        id: impl Into<ServiceID>,
        config_dir: &Path,
        config: config::Service,
    ) -> eyre::Result<Self> {
        let (prog, args) = config.command;
        let id: ServiceID = id.into();

        let working_dir = config
            .working_dir
            .as_ref()
            .map(|dir| env::resolve_path(config_dir, dir.as_ref()))
            .transpose()?;

        let env_files = config
            .env_file
            .iter()
            .map(|env_file| env::resolve_path(config_dir, env_file.path.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;

        let base_env: std::collections::HashMap<String, String> = std::env::vars().collect();
        let env_file_env = env::load_env_files_sync(&env_files)?;
        let env_file_env = env::expand_env_values(&env_file_env, &base_env);

        let mut base_with_env_file = base_env.clone();
        for (k, v) in env_file_env.iter() {
            base_with_env_file.insert(k.clone(), v.clone());
        }

        let mut config_env_map = env::EnvMap::new();
        for (k, v) in &config.environment {
            config_env_map.insert(k.as_ref().clone(), v.as_ref().clone());
        }
        let config_env_map = env::expand_env_values(&config_env_map, &base_with_env_file);

        let mut full_env = base_with_env_file.clone();
        for (k, v) in config_env_map.iter() {
            full_env.insert(k.clone(), v.clone());
        }

        let open_ports = config
            .ports
            .iter()
            .map(|port| {
                let expanded = env::interpolate_str(port.as_ref(), &full_env);
                expanded
                    .parse::<u16>()
                    .map_err(|err| eyre::eyre!("invalid port `{}`: {err}", expanded))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut environment = indexmap::IndexMap::new();
        for (k, v) in env_file_env.iter() {
            environment.insert(k.clone(), v.clone());
        }
        for (k, v) in config_env_map.iter() {
            environment.insert(k.clone(), v.clone());
        }

        Ok(Self {
            id,
            name: config.name,
            command: (
                prog.into_inner(),
                args.into_iter()
                    .map(|value| value.to_string())
                    .collect::<Vec<_>>(),
            ),
            working_dir,
            open_ports,
            restart_policy: config.restart.unwrap_or_default(),
            depends_on: config.depends_on,
            environment,
            health_check: config.healthcheck,
            enable_color: config.color.as_deref().copied().unwrap_or(true),
        })
    }
}
