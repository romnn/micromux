use std::time::Duration;

use async_process::{Command, Stdio};
use color_eyre::eyre;
use futures::stream::StreamExt;
use futures::{AsyncBufReadExt, channel::mpsc};
use itertools::Itertools;
use yaml_spanned::Spanned;

use crate::{
    config::{self, HealthCheck},
    health_check::Health,
    scheduler::{ServiceID, State},
};

// #[derive(
//     Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, strum::Display, strum::IntoStaticStr,
// )]
// pub enum State {
//     #[strum(serialize = "PENDING")]
//     Pending,
//     #[strum(serialize = "RUNNING")]
//     Running,
//     #[strum(serialize = "EXITED")]
//     Exited,
//     #[strum(serialize = "DISABLED")]
//     Disabled,
// }

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

// #[derive(Debug)]
// pub struct ProcessState {
//     pub(crate) process: Option<async_process::Child>,
//     pub(crate) stdout_tx: mpsc::Sender<String>,
//     pub stdout_rx: mpsc::Receiver<String>,
//     pub(crate) stderr_tx: mpsc::Sender<String>,
//     pub stderr_rx: mpsc::Receiver<String>,
// }

#[derive(Debug)]
pub struct Service {
    pub id: ServiceID,
    pub name: Spanned<String>,
    pub command: (String, Vec<String>),
    pub restart_policy: RestartPolicy,
    pub depends_on: Vec<config::Dependency>,
    pub health_check: Option<config::HealthCheck>,
    pub state: State,
    pub health: Option<Health>,
    pub open_ports: Vec<u16>,
    // pub process_state: ProcessState,
    pub(crate) process: Option<async_process::Child>,
    // pub(crate) stdout_tx: mpsc::Sender<Result<String, std::io::Error>>,
    // pub stdout_rx: mpsc::Receiver<Result<String, std::io::Error>>,
    // pub(crate) stderr_tx: mpsc::Sender<Result<String, std::io::Error>>,
    // pub stderr_rx: mpsc::Receiver<Result<String, std::io::Error>>,
}

impl Service {
    pub fn new(id: impl Into<ServiceID>, config: config::Service) -> Self {
        let (prog, args) = config.command;
        // let (stdout_tx, stdout_rx) = mpsc::channel(1024);
        // let (stderr_tx, stderr_rx) = mpsc::channel(1024);
        Self {
            id: id.into(),
            name: config.name,
            // command: config.command.iter().map(|part| part.as_str()).join(" "),
            command: (
                prog.into_inner(),
                args.into_iter()
                    .map(|value| value.to_string())
                    .collect::<Vec<_>>(),
            ),
            open_ports: config.ports.clone(),
            restart_policy: config.restart.unwrap_or_default(),
            depends_on: config.depends_on,
            health_check: config.healthcheck,
            state: State::Pending,
            health: None,
            process: None,
            // stdout_tx,
            // stdout_rx,
            // stderr_tx,
            // stderr_rx,
        }
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
