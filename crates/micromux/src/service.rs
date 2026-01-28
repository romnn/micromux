use async_process::{Command, Stdio};
use color_eyre::eyre;
use futures::stream::StreamExt;
use futures::{AsyncBufReadExt, channel::mpsc};
use itertools::Itertools;
use std::path::{Path, PathBuf};
use std::time::Duration;
use yaml_spanned::Spanned;

use crate::{
    config::{self, HealthCheck},
    env,
    health_check::Health,
    scheduler::{ServiceID, State},
};

/// Send a Unix signal to a process with the given PID.
#[cfg(unix)]
pub fn send_signal(pid: u32, sig: nix::sys::signal::Signal) -> eyre::Result<()> {
    // Send SIGTERM to child process.
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), sig)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use indexmap::IndexMap;
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};
    use yaml_spanned::Spanned;

    fn spanned_string(value: &str) -> Spanned<String> {
        Spanned {
            span: Default::default(),
            inner: value.to_string(),
        }
    }

    fn unique_tmp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
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
    fn env_file_missing_is_error() {
        let dir = unique_tmp_dir("env-missing");
        std::fs::create_dir_all(&dir).unwrap();
        let mut cfg = service_config("svc", ("sh", &["-c", "true"]));
        cfg.env_file = vec![config::EnvFile {
            path: spanned_string("./definitely-not-present.env"),
        }];

        let err = Service::new("svc", &dir, cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("failed to read env file"), "{msg}");
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

        let err = Service::new("svc", &dir, cfg).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("failed to parse env file"), "{msg}");
        Ok(())
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
            svc.environment.get("FOO").map(|s| s.as_str()),
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
            svc.environment.get("PORT").map(|s| s.as_str()),
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
        assert_eq!(svc.env_files, vec![dir.join(".env")]);
        assert_eq!(
            svc.environment
                .get("AIRTYPE_API_SPICEDB_ENDPOINT")
                .map(|s| s.as_str()),
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
    pub env_files: Vec<PathBuf>,
    pub environment: indexmap::IndexMap<String, String>,
    pub health_check: Option<config::HealthCheck>,
    pub state: State,
    pub health: Option<Health>,
    pub open_ports: Vec<u16>,
    pub enable_color: bool,
    pub(crate) process: Option<async_process::Child>,
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
        for (k, v) in config.environment.iter() {
            config_env_map.insert(k.as_ref().to_string(), v.as_ref().to_string());
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
            // command: config.command.iter().map(|part| part.as_str()).join(" "),
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
            env_files,
            environment,
            health_check: config.healthcheck,
            state: State::Pending,
            health: None,
            process: None,
            enable_color: config.color.as_deref().copied().unwrap_or(true),
        })
    }

    pub fn with_state(mut self, state: impl Into<State>) -> Self {
        self.state = state.into();
        self
    }

    pub fn with_health(mut self, health: impl Into<Health>) -> Self {
        self.health = Some(health.into());
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.open_ports.push(port);
        self
    }

    // pub fn is_healthy(&self) -> bool {
    //     match self.health {
    //         Some(health) => health == Health::Healthy,
    //         None => self.state == State::Running,
    //     }
    // }

    pub async fn terminate(&mut self, timeout: Duration) -> eyre::Result<()> {
        let Some(mut process) = self.process.take() else {
            return Ok(());
        };
        let pid = process.id();
        tracing::debug!(pid, "sending SIGTERM");

        #[cfg(unix)]
        send_signal(pid, nix::sys::signal::Signal::SIGTERM)?;

        #[cfg(not(unix))]
        panic!("termination is not yet implemented on windows");

        // wait up to 10 seconds for the child to exit gracefully.
        match tokio::time::timeout(timeout, process.status()).await {
            Ok(status_result) => {
                let status = status_result?;
                tracing::debug!(?status, pid, "process exited");
            }
            Err(_elapsed) => {
                tracing::debug!(pid, "killing process");
                process.kill()?;
            }
        }
        Ok(())
    }

    // pub async fn spawn(&mut self) -> eyre::Result<()> {
    //     let args: Vec<String> = shlex::split(&self.command).unwrap_or_default();
    //     let Some((program, program_args)) = args.split_first() else {
    //         eyre::bail!("bad command: {:?}", self.command);
    //     };
    //     let mut process = Command::new(program)
    //         .args(program_args)
    //         .stdout(Stdio::piped())
    //         .spawn()?;
    //
    //     if let Some(stdout) = process.stdout.take() {
    //         // let mut lines = tokio::io::BufReader::new(stdout).lines();
    //         let mut lines = futures::io::BufReader::new(stdout).lines();
    //
    //         while let Some(line) = lines.next().await {
    //             println!("{}", line?);
    //         }
    //     }
    //
    //     self.process = Some(process);
    //     Ok(())
    // }

    // pub async fn run_health_check(&mut self) -> eyre::Result<()> {
    //     let Some(health_check) = &self.health_check else {
    //         return Ok(());
    //     };
    //     health_check.run(&self.id, shutdown_handle).await?;
    //
    //     // self.process = Some(process);
    //     Ok(())
    // }

    // self.state_change.notify_waiters();
}
