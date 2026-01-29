use crate::scheduler::{Event, OutputStream, ServiceID};
use itertools::Itertools;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, strum::Display)]
pub enum Health {
    Healthy,
    Unhealthy,
}

#[cfg(test)]
mod tests {
    use super::*;
    use yaml_spanned::Spanned;

    fn spanned_string(value: &str) -> Spanned<String> {
        Spanned {
            span: Default::default(),
            inner: value.to_string(),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_child_and_emits_finished() -> color_eyre::eyre::Result<()> {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        use std::time::{SystemTime, UNIX_EPOCH};

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("micromux-hc-timeout-{nanos}"));
        std::fs::create_dir_all(&dir)?;
        let pid_path = dir.join("pid");

        let hc = crate::config::HealthCheck {
            test: (
                spanned_string("sh"),
                vec![
                    spanned_string("-c"),
                    spanned_string(&format!(
                        "echo $$ > {} && sleep 5",
                        pid_path.to_string_lossy()
                    )),
                ],
            ),
            start_delay: None,
            interval: None,
            timeout: Some(Spanned {
                span: Default::default(),
                inner: std::time::Duration::from_millis(50),
            }),
            retries: Some(Spanned {
                span: Default::default(),
                inner: 1,
            }),
        };

        let (events_tx, mut events_rx) = mpsc::channel(64);
        let shutdown = CancellationToken::new();
        let terminate = CancellationToken::new();
        let env = std::collections::HashMap::new();

        let start = tokio::time::Instant::now();
        let res = super::run(
            &hc,
            &"svc".to_string(),
            1,
            Some(&dir),
            &env,
            events_tx,
            shutdown,
            terminate,
        )
        .await;
        assert!(matches!(
            res,
            Err(Error {
                source: ErrorReason::Timeout,
                ..
            })
        ));
        assert!(start.elapsed() < std::time::Duration::from_secs(1));

        let pid_str = std::fs::read_to_string(&pid_path)?;
        let pid: i32 = pid_str.trim().parse()?;

        let mut saw_finished = false;
        for _ in 0..50 {
            if let Some(ev) = events_rx.recv().await
                && let Event::HealthCheckFinished { success: false, .. } = ev
            {
                saw_finished = true;
                break;
            }
        }
        assert!(saw_finished);

        match kill(Pid::from_raw(pid), None) {
            Err(Errno::ESRCH) => {}
            other => color_eyre::eyre::bail!("expected ESRCH for dead pid, got {other:?}"),
        }

        Ok(())
    }
}

#[derive(thiserror::Error, Debug)]
pub enum ErrorReason {
    #[error("timeout")]
    Timeout,
    #[error("failed with non-zero exit code {exit_code}")]
    Failed { exit_code: i32 },
    #[error("failed to spawn")]
    Spawn(#[from] std::io::Error),
}

#[derive(thiserror::Error, Debug)]
#[error("healthcheck `{command}` failed")]
pub struct Error {
    pub command: String,
    #[source]
    pub source: ErrorReason,
}

pub async fn run_loop(
    health_check: crate::config::HealthCheck,
    service_id: &ServiceID,
    working_dir: Option<std::path::PathBuf>,
    environment: std::collections::HashMap<String, String>,
    events_tx: mpsc::Sender<Event>,
    shutdown: CancellationToken,
    terminate: CancellationToken,
) {
    let max_retries = health_check.retries.as_deref().copied().unwrap_or(1);
    let start_delay = health_check
        .start_delay
        .as_deref()
        .cloned()
        .unwrap_or_default();
    let interval = health_check.interval.as_deref().cloned().unwrap_or_default();
    tracing::info!(
        service_id,
        ?start_delay,
        ?interval,
        max_retries,
        "starting health check loop"
    );

    if !start_delay.is_zero() {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = terminate.cancelled() => return,
            _ = tokio::time::sleep(start_delay) => {},
        };
    }

    let mut attempt = 0;
    let mut run_id: u64 = 0;
    loop {
        run_id = run_id.wrapping_add(1);
        let attempt_id = run_id;

        let res = run(
            &health_check,
            service_id,
            attempt_id,
            working_dir.as_deref(),
            &environment,
            events_tx.clone(),
            shutdown.clone(),
            terminate.clone(),
        )
        .await;
        match res {
            Ok(()) => {
                let _ = events_tx.send(Event::Healthy(service_id.to_string())).await;
                attempt = 0;
            }
            Err(err) => {
                match err.source {
                    ErrorReason::Failed { exit_code } => {
                        tracing::warn!(
                            service_id,
                            code = exit_code,
                            attempt,
                            max_attempts = max_retries,
                            "health check failed",
                        );
                    }
                    ErrorReason::Spawn(err) => {
                        tracing::warn!(
                            ?err,
                            service_id,
                            attempt,
                            max_attempts = max_retries,
                            "failed to run health check",
                        );
                    }
                    ErrorReason::Timeout => {
                        tracing::warn!(
                            service_id,
                            attempt,
                            max_attempts = max_retries,
                            "health check timed out",
                        );
                    }
                };

                if attempt < max_retries {
                    attempt = attempt.saturating_add(1);
                } else {
                    let _ = events_tx
                        .send(Event::Unhealthy(service_id.to_string()))
                        .await;
                    return;
                }
            }
        }

        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = terminate.cancelled() => return,
            _ = tokio::time::sleep(interval) => {},
        };
    }
}

pub async fn run(
    health_check: &crate::config::HealthCheck,
    service_id: &ServiceID,
    attempt: u64,
    working_dir: Option<&std::path::Path>,
    environment: &std::collections::HashMap<String, String>,
    events_tx: mpsc::Sender<Event>,
    shutdown: CancellationToken,
    terminate: CancellationToken,
) -> Result<(), Error> {
        let (prog, args) = &health_check.test;
        let command_string = || {
            [prog]
                .into_iter()
                .chain(args.iter())
                .map(|value| value.as_str())
                .join(" ")
        };

        let _ = events_tx
            .send(Event::HealthCheckStarted {
                service_id: service_id.to_string(),
                attempt,
                command: command_string(),
            })
            .await;

        let mut cmd = Command::new(prog.as_ref());
        cmd.args(args.iter().map(|value| value.as_ref()))
            .envs(environment.iter())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped());
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        if let Some(dir) = working_dir {
            cmd.current_dir(dir);
        }
        let mut process = cmd.spawn().map_err(|source| {
            let _ = events_tx.try_send(Event::HealthCheckLogLine {
                service_id: service_id.to_string(),
                attempt,
                stream: OutputStream::Stderr,
                line: source.to_string(),
            });
            let _ = events_tx.try_send(Event::HealthCheckFinished {
                service_id: service_id.to_string(),
                attempt,
                success: false,
                exit_code: -1,
            });
            Error {
                command: command_string(),
                source: ErrorReason::Spawn(source),
            }
        })?;

        let mut stderr_task = None;
        if let Some(stderr) = process.stderr.take() {
            let service_id = service_id.clone();
            let events_tx = events_tx.clone();
            stderr_task = Some(tokio::task::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            let _ = events_tx
                                .send(Event::HealthCheckLogLine {
                                    service_id: service_id.to_string(),
                                    attempt,
                                    stream: OutputStream::Stderr,
                                    line,
                                })
                                .await;
                        }
                        Ok(None) => break,
                        Err(err) => {
                            tracing::error!(service_id, ?err, "health check: failed to read line")
                        }
                    }
                }
            }));
        }

        let mut stdout_task = None;
        if let Some(stdout) = process.stdout.take() {
            let service_id = service_id.clone();
            let events_tx = events_tx.clone();
            stdout_task = Some(tokio::task::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            let _ = events_tx
                                .send(Event::HealthCheckLogLine {
                                    service_id: service_id.to_string(),
                                    attempt,
                                    stream: OutputStream::Stdout,
                                    line,
                                })
                                .await;
                        }
                        Ok(None) => break,
                        Err(err) => {
                            tracing::error!(service_id, ?err, "health check: failed to read line")
                        }
                    }
                }
            }));
        }

        let child_pid = process.id().map(|pid| pid as i32);
        let kill_token = CancellationToken::new();
        let mut child = process;
        let kill_token_child = kill_token.clone();
        let poll = std::time::Duration::from_millis(25);
        let mut wait_handle = tokio::task::spawn(async move {
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => return Ok(status),
                    Ok(None) => {}
                    Err(err) => return Err(err),
                }

                tokio::select! {
                    _ = kill_token_child.cancelled() => {
                        #[cfg(unix)]
                        if let Some(pid) = child.id() {
                            let pid = nix::unistd::Pid::from_raw(pid as i32);
                            let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
                        }
                        let _ = child.kill();
                        return child.wait().await;
                    }
                    _ = tokio::time::sleep(poll) => {}
                }
            }
        });

        let timeout = health_check.timeout.clone().map(|t| t.into_inner());

        enum Completion {
            Shutdown,
            Timeout,
            Status(Result<std::process::ExitStatus, std::io::Error>),
        }

        let completion = tokio::select! {
            _ = shutdown.cancelled() => Completion::Shutdown,
            _ = terminate.cancelled() => Completion::Shutdown,
            _ = async {
                if let Some(d) = timeout {
                    tokio::time::sleep(d).await;
                } else {
                    futures::future::pending::<()>().await;
                }
            } => Completion::Timeout,
            res = &mut wait_handle => Completion::Status(res.unwrap_or_else(|err| Err(std::io::Error::other(err.to_string())))),
        };

        match completion {
            Completion::Shutdown => {
                kill_token.cancel();
                #[cfg(unix)]
                if let Some(pid) = child_pid {
                    let pid = nix::unistd::Pid::from_raw(pid);
                    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
                    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
                }
                let _ =
                    tokio::time::timeout(std::time::Duration::from_secs(1), &mut wait_handle).await;
                if let Some(task) = stdout_task.take() {
                    let _ = task.await;
                }
                if let Some(task) = stderr_task.take() {
                    let _ = task.await;
                }
                let _ = events_tx
                    .send(Event::HealthCheckFinished {
                        service_id: service_id.to_string(),
                        attempt,
                        success: false,
                        exit_code: -1,
                    })
                    .await;
                Ok(())
            }
            Completion::Timeout => {
                kill_token.cancel();
                #[cfg(unix)]
                if let Some(pid) = child_pid {
                    let pid = nix::unistd::Pid::from_raw(pid);
                    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
                    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
                }
                let _ =
                    tokio::time::timeout(std::time::Duration::from_secs(1), &mut wait_handle).await;
                if let Some(task) = stdout_task.take() {
                    let _ = task.await;
                }
                if let Some(task) = stderr_task.take() {
                    let _ = task.await;
                }
                let _ = events_tx
                    .send(Event::HealthCheckFinished {
                        service_id: service_id.to_string(),
                        attempt,
                        success: false,
                        exit_code: -1,
                    })
                    .await;
                Err(Error {
                    command: command_string(),
                    source: ErrorReason::Timeout,
                })
            }
            Completion::Status(Ok(status)) if status.success() => {
                if let Some(task) = stdout_task.take() {
                    let _ = task.await;
                }
                if let Some(task) = stderr_task.take() {
                    let _ = task.await;
                }
                let _ = events_tx
                    .send(Event::HealthCheckFinished {
                        service_id: service_id.to_string(),
                        attempt,
                        success: true,
                        exit_code: status.code().unwrap_or(0),
                    })
                    .await;
                Ok(())
            }
            Completion::Status(Ok(status)) => {
                if let Some(task) = stdout_task.take() {
                    let _ = task.await;
                }
                if let Some(task) = stderr_task.take() {
                    let _ = task.await;
                }
                let exit_code = status.code().unwrap_or(-1);
                let _ = events_tx
                    .send(Event::HealthCheckFinished {
                        service_id: service_id.to_string(),
                        attempt,
                        success: false,
                        exit_code,
                    })
                    .await;
                Err(Error {
                    command: command_string(),
                    source: ErrorReason::Failed { exit_code },
                })
            }
            Completion::Status(Err(err)) => {
                if let Some(task) = stdout_task.take() {
                    let _ = task.await;
                }
                if let Some(task) = stderr_task.take() {
                    let _ = task.await;
                }
                let _ = events_tx
                    .send(Event::HealthCheckFinished {
                        service_id: service_id.to_string(),
                        attempt,
                        success: false,
                        exit_code: -1,
                    })
                    .await;
                Err(Error {
                    command: command_string(),
                    source: ErrorReason::Spawn(err),
                })
            }
        }
    }

