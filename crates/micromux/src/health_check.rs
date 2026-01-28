use crate::scheduler::{Event, OutputStream, ServiceID};
use color_eyre::eyre;
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt};
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
    async fn timeout_kills_child_and_emits_finished() -> eyre::Result<()> {
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
        let res = hc
            .run(
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
            if let Some(ev) = events_rx.recv().await {
                if let Event::HealthCheckFinished { success: false, .. } = ev {
                    saw_finished = true;
                    break;
                }
            }
        }
        assert!(saw_finished);

        match kill(Pid::from_raw(pid), None) {
            Err(Errno::ESRCH) => {}
            other => eyre::bail!("expected ESRCH for dead pid, got {other:?}"),
        }

        Ok(())
    }
}

// #[derive(thiserror::Error, Debug)]
// #[error("bad command")]
// pub struct BadCommandError;

#[derive(thiserror::Error, Debug)]
pub enum ErrorReason {
    #[error("timeout")]
    Timeout,
    #[error("failed with non-zero exit code {exit_code}")]
    Failed { exit_code: i32 },
    #[error("failed to spawn")]
    Spawn(#[from] std::io::Error),
    // #[error("bad command")]
    // BadCommand(BadCommandError),
}

#[derive(thiserror::Error, Debug)]
// #[error("healthcheck `{command}` failed (attempt {attempt}/{max_attempts})")]
#[error("healthcheck `{command}` failed")]
pub struct Error {
    pub command: String,
    // pub attempt: usize,
    // pub max_attempts: usize,
    #[source]
    pub source: ErrorReason,
}

impl crate::config::HealthCheck {
    pub async fn run_loop(
        self,
        service_id: &ServiceID,
        working_dir: Option<std::path::PathBuf>,
        environment: std::collections::HashMap<String, String>,
        events_tx: mpsc::Sender<Event>,
        // mut shutdown_handle: crate::shutdown::Handle,
        shutdown: CancellationToken,
        terminate: CancellationToken,
        // ) -> Result<(), BadCommandError> {
    ) {
        let max_retries = self.retries.as_deref().copied().unwrap_or(1);
        let start_delay = self.start_delay.as_deref().cloned().unwrap_or_default();
        let interval = self.interval.as_deref().cloned().unwrap_or_default();
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

            let mut shutdown_clone = shutdown.clone();
            let res = self
                .run(
                    service_id,
                    attempt_id,
                    working_dir.as_deref(),
                    &environment,
                    events_tx.clone(),
                    shutdown.clone(),
                    terminate.clone(),
                )
                .await;
            // let res = tokio::select! {
            //     _ = shutdown_clone.cancelled() => {
            //         tracing::info!(service_id, "shutting down health check");
            //         return;
            //         // return Ok(())
            //     }
            //     res = self.run(service_id, shutdown.clone(), terminate.clone()) => res,
            // };
            match res {
                Ok(()) => {
                    let _ = events_tx.send(Event::Healthy(service_id.to_string())).await;
                    // Reset attempts
                    attempt = 0;
                }
                Err(err) => {
                    // tracing::warn!(?err, ?attempt);
                    let command = err.command;
                    match err.source {
                        ErrorReason::Failed { exit_code } => {
                            tracing::warn!(
                                service_id,
                                code = exit_code,
                                // command,
                                attempt,
                                max_attempts = max_retries,
                                "health check failed",
                            );
                        }
                        ErrorReason::Spawn(err) => {
                            tracing::warn!(
                                ?err,
                                service_id,
                                // command,
                                attempt,
                                max_attempts = max_retries,
                                "failed to run health check",
                            );
                        }
                        ErrorReason::Timeout => {
                            tracing::warn!(
                                // command,
                                service_id,
                                attempt,
                                max_attempts = max_retries,
                                "health check timed out",
                            );
                        } // ErrorReason::BadCommand(err) => {
                          //     // we cannot recover if the command is invalid
                          //     return Err(err);
                          // }
                    };

                    if attempt < max_retries {
                        // Increment attempt
                        attempt = attempt.saturating_add(1);
                        // tokio::select! {
                        //     _ = cancel.cancelled() => return Ok(()),
                        //     _ = tokio::time::sleep(interval) => {},
                        // };
                        // continue;
                    } else {
                        let _ = events_tx
                            .send(Event::Unhealthy(service_id.to_string()))
                            .await;
                        return;
                        // return Ok(());
                        // // Reset attempts
                        // attempt = 0;
                    }
                    // tracing::warn!(?attempt, ?max_retries);
                }
            }

            // Wait the full interval before re-checking
            tokio::select! {
                // _ = cancel.cancelled() => return Ok(()),
                _ = shutdown.cancelled() => return,
                _ = terminate.cancelled() => return,
                _ = tokio::time::sleep(interval) => {},
            };
            // tracing::debug!(?interval, "slept");
        }
    }

    pub async fn run(
        &self,
        service_id: &ServiceID,
        attempt: u64,
        working_dir: Option<&std::path::Path>,
        environment: &std::collections::HashMap<String, String>,
        events_tx: mpsc::Sender<Event>,
        // mut shutdown_handle: crate::shutdown::Handle,
        shutdown: CancellationToken,
        terminate: CancellationToken,
    ) -> Result<(), Error> {
        // let command: Vec<&str> = self.test.iter().map(|part| part.as_str()).collect();
        // let command_string = || command.join(" ");
        let (prog, args) = &self.test;
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

        // let args: Vec<String> = shlex::split(&test_command).unwrap_or_default();
        // let Some((program, program_args)) = command.split_first() else {
        //     return Err(Error {
        //         command: command_string(),
        //         // attempt: 1,
        //         // max_attempts: max_retries,
        //         source: ErrorReason::BadCommand(BadCommandError),
        //     });
        // };

        // for attempt in 1..=max_retries {
        let mut cmd = Command::new(prog.as_ref());
        cmd.args(args.iter().map(|value| value.as_ref()))
            .envs(environment.iter())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
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

        let command = command_string();

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

        let timeout = self.timeout.clone().map(|t| t.into_inner());

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
            res = &mut wait_handle => Completion::Status(res.unwrap_or_else(|err| Err(std::io::Error::new(std::io::ErrorKind::Other, err.to_string())))),
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

        // if attempt < max_retries {
        //     if let Some(interval) = self.interval.clone() {
        //         tokio::time::sleep(interval.into_inner()).await;
        //     }
        // }
        // }

        // Err(Error {
        //     command: command_string(),
        //     attempt: 1,
        //     max_attempts: max_retries,
        //     source: ErrorReason::BadCommand,
        // })
        // Err(eyre::eyre!(
        //     "healthcheck [{} {:?}] failed after {} attempts",
        //     prog,
        //     args,
        //     max_retries
        // ))
    }

    // pub async fn run(&self, service_name: &str) -> Result<(), Error> {
    //     let max_retries = self.retries.as_deref().copied().unwrap_or(1);
    //
    //     let test_command = self.test.iter().map(|part| part.as_str()).join(" ");
    //     let args: Vec<String> = shlex::split(&test_command).unwrap_or_default();
    //
    //     let Some((program, program_args)) = args.split_first() else {
    //         return Err(Error {
    //             command: test_command,
    //             attempt: 1,
    //             max_attempts: max_retries,
    //             source: ErrorReason::BadCommand,
    //         });
    //     };
    //
    //     for attempt in 1..=max_retries {
    //         let mut process = Command::new(program)
    //             .args(program_args)
    //             .stdout(Stdio::piped())
    //             .spawn()
    //             .map_err(|source| Error {
    //                 command: test_command.clone(),
    //                 attempt,
    //                 max_attempts: max_retries,
    //                 source: ErrorReason::Spawn(source),
    //             })?;
    //
    //         if let Some(stdout) = process.stdout.take() {
    //             // let mut lines = tokio::io::BufReader::new(stdout).lines();
    //             let mut lines = futures::io::BufReader::new(stdout).lines();
    //
    //             while let Some(line) = lines.next().await {
    //                 match line {
    //                     Ok(line) => tracing::trace!(name = service_name, "health check: {}", line,),
    //                     Err(err) => {
    //                         tracing::error!(
    //                             name = service_name,
    //                             ?err,
    //                             "health check: failed to read line"
    //                         )
    //                     }
    //                 }
    //             }
    //         }
    //
    //         let res = match self.timeout.clone() {
    //             Some(timeout) => tokio::time::timeout(timeout.into_inner(), process.status()).await,
    //             None => {
    //                 process
    //                     .status()
    //                     .map(Result::<_, tokio::time::error::Elapsed>::Ok)
    //                     .await
    //             }
    //         };
    //         match res {
    //             Ok(Ok(status)) if status.success() => {
    //                 // healthcheck passed
    //                 return Ok(());
    //             }
    //             Ok(Ok(status)) => {
    //                 // command ran but returned non-zero
    //                 tracing::warn!(
    //                     code = status.code(),
    //                     command = test_command,
    //                     attempt = attempt,
    //                     max_attempts = max_retries,
    //                     "health check failed",
    //                     // "health check `{}` failed with exit code {:?} (attempt {}/{})",
    //                     // test_command,
    //                     // status.code(),
    //                     // attempt,
    //                     // max_retries
    //                 );
    //             }
    //             Ok(Err(err)) => {
    //                 // spawn / execution error
    //                 tracing::warn!(
    //                     ?err,
    //                     command = test_command,
    //                     attempt = attempt,
    //                     max_attempts = max_retries,
    //                     "health check failed",
    //                     // "health check `{}` failed to spawn: {} (attempt {}/{})",
    //                     // test_command,
    //                     // err,
    //                     // attempt,
    //                     // max_retries
    //                 );
    //             }
    //             Err(_) => {
    //                 // timeout elapsed
    //                 tracing::warn!(
    //                     command = test_command,
    //                     attempt = attempt,
    //                     max_attempts = max_retries,
    //                     "health check timed out",
    //                     // "healthcheck [{} {:?}] timed out after {:?} (attempt {}/{})",
    //                     // prog,
    //                     // args,
    //                     // timeout_dur,
    //                     // attempt,
    //                     // max_retries
    //                 );
    //             }
    //         }
    //
    //         if attempt < max_retries {
    //             if let Some(interval) = self.interval.clone() {
    //                 tokio::time::sleep(interval.into_inner()).await;
    //             }
    //         }
    //     }
    //
    //     Err(Error {
    //         command: test_command,
    //         attempt: 1,
    //         max_attempts: max_retries,
    //         source: ErrorReason::BadCommand,
    //     })
    //     // Err(eyre::eyre!(
    //     //     "healthcheck [{} {:?}] failed after {} attempts",
    //     //     prog,
    //     //     args,
    //     //     max_retries
    //     // ))
    // }
}
