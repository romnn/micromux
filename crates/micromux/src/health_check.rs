use crate::scheduler::{Event, ServiceID};
use async_process::{Command, Stdio};
use color_eyre::eyre;
use futures::{AsyncBufReadExt, FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use itertools::Itertools;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, strum::Display)]
pub enum Health {
    Healthy,
    Unhealthy,
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
        events_tx: mpsc::Sender<Event>,
        // mut shutdown_handle: crate::shutdown::Handle,
        shutdown: CancellationToken,
        terminate: CancellationToken,
        // ) -> Result<(), BadCommandError> {
    ) {
        let max_retries = self.retries.as_deref().copied().unwrap_or(1);
        let interval = self.interval.as_deref().cloned().unwrap_or_default();
        tracing::info!(
            service_id,
            ?interval,
            max_retries,
            "starting health check loop"
        );

        let mut attempt = 0;
        loop {
            let mut shutdown_clone = shutdown.clone();
            let res = self
                .run(service_id, shutdown.clone(), terminate.clone())
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
        let mut process = Command::new(prog.as_ref())
            .args(args.iter().map(|value| value.as_ref()))
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|source| Error {
                command: command_string(),
                // attempt,
                // max_attempts: max_retries,
                source: ErrorReason::Spawn(source),
            })?;

        if let Some(stderr) = process.stderr.take() {
            let service_id = service_id.clone();
            tokio::task::spawn(async move {
                let mut lines = futures::io::BufReader::new(stderr).lines();

                while let Some(line) = lines.next().await {
                    match line {
                        Ok(line) => tracing::trace!(service_id, "health check: {}", line),
                        Err(err) => {
                            tracing::error!(service_id, ?err, "health check: failed to read line")
                        }
                    }
                }
            });
        }

        if let Some(stdout) = process.stdout.take() {
            let service_id = service_id.clone();
            tokio::task::spawn(async move {
                let mut lines = futures::io::BufReader::new(stdout).lines();

                while let Some(line) = lines.next().await {
                    match line {
                        Ok(line) => tracing::trace!(service_id, "health check: {}", line),
                        Err(err) => {
                            tracing::error!(service_id, ?err, "health check: failed to read line")
                        }
                    }
                }
            });
        }

        let process_fut = match self.timeout.clone() {
            Some(timeout) => tokio::time::timeout(timeout.into_inner(), process.status()).boxed(),
            None => process
                .status()
                .map(Result::<_, tokio::time::error::Elapsed>::Ok)
                .boxed(),
        };

        // service_id: ServiceID,
        // events_tx: mpsc::Sender<Event>
        let kill = |mut process: async_process::Child| async move {
            tracing::info!(
                pid = process.id(),
                command = command_string(),
                "killing process"
            );
            // Kill the process
            let _ = process.kill();
            // Optionally wait for it to actually exit
            let _ = process.status().await;
            // return Ok(());
            // let _ = events_tx.send(Event::Exited(service_id.clone(), -1)).await;
        };

        let res = tokio::select! {
            _ = shutdown.cancelled() => {
                kill(process).await;
                return Ok(());
            }
            _ = terminate.cancelled() => {
                kill(process).await;
                return Ok(());
            }
            status = process_fut => status,
        };

        // let res = match self.timeout.clone() {
        //     Some(timeout) => tokio::time::timeout(timeout.into_inner(), process.status()).await,
        //     None => {
        //         process
        //             .status()
        //             .map(Result::<_, tokio::time::error::Elapsed>::Ok)
        //             .await
        //     }
        // };
        match res {
            Ok(Ok(status)) if status.success() => {
                // healthcheck passed
                Ok(())
            }
            Ok(Ok(status)) => {
                // command ran but returned non-zero
                Err(Error {
                    command: command_string(),
                    source: ErrorReason::Failed {
                        exit_code: status.code().unwrap_or(-1),
                    },
                })
            }
            Ok(Err(err)) => {
                // spawn / execution error
                Err(Error {
                    command: command_string(),
                    source: ErrorReason::Spawn(err),
                })
            }
            Err(_) => {
                // timeout elapsed
                Err(Error {
                    command: command_string(),
                    source: ErrorReason::Timeout,
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
