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
