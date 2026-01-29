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
            span: yaml_spanned::spanned::Span::default(),
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
            .unwrap_or_default()
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
                span: yaml_spanned::spanned::Span::default(),
                inner: std::time::Duration::from_millis(50),
            }),
            retries: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: 1,
            }),
        };

        let (events_tx, mut events_rx) = mpsc::channel(64);
        let shutdown = CancellationToken::new();
        let terminate = CancellationToken::new();
        let env = std::collections::HashMap::new();
        let service_id: ServiceID = "svc".to_string();

        let start = tokio::time::Instant::now();
        let res = super::run(
            &hc,
            &service_id,
            1,
            RunParams {
                working_dir: Some(&dir),
                environment: &env,
                events_tx,
                shutdown,
                terminate,
            },
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

struct RunParams<'a> {
    pub working_dir: Option<&'a std::path::Path>,
    pub environment: &'a std::collections::HashMap<String, String>,
    pub events_tx: mpsc::Sender<Event>,
    pub shutdown: CancellationToken,
    pub terminate: CancellationToken,
}

enum Completion {
    Shutdown,
    Timeout,
    Status(Result<std::process::ExitStatus, std::io::Error>),
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
        .copied()
        .unwrap_or_default();
    let interval = health_check.interval.as_deref().copied().unwrap_or_default();
    tracing::info!(
        service_id,
        ?start_delay,
        ?interval,
        max_retries,
        "starting health check loop"
    );

    if !start_delay.is_zero() {
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = terminate.cancelled() => return,
            () = tokio::time::sleep(start_delay) => {},
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
            RunParams {
                working_dir: working_dir.as_deref(),
                environment: &environment,
                events_tx: events_tx.clone(),
                shutdown: shutdown.clone(),
                terminate: terminate.clone(),
            },
        )
        .await;
        match res {
            Ok(()) => {
                let _ = events_tx.send(Event::Healthy(service_id.clone())).await;
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
                }

                if attempt < max_retries {
                    attempt = attempt.saturating_add(1);
                } else {
                    let _ = events_tx
                        .send(Event::Unhealthy(service_id.clone()))
                        .await;
                    return;
                }
            }
        }

        tokio::select! {
            () = shutdown.cancelled() => return,
            () = terminate.cancelled() => return,
            () = tokio::time::sleep(interval) => {},
        };
    }
}

fn command_string(health_check: &crate::config::HealthCheck) -> String {
    let (prog, args) = &health_check.test;
    [prog]
        .into_iter()
        .chain(args.iter())
        .map(|value| value.as_str())
        .join(" ")
}

async fn emit_started(
    events_tx: &mpsc::Sender<Event>,
    service_id: &ServiceID,
    attempt: u64,
    command: String,
) {
    let _ = events_tx
        .send(Event::HealthCheckStarted {
            service_id: service_id.clone(),
            attempt,
            command,
        })
        .await;
}

async fn emit_finished(
    events_tx: &mpsc::Sender<Event>,
    service_id: &ServiceID,
    attempt: u64,
    success: bool,
    exit_code: i32,
) {
    let _ = events_tx
        .send(Event::HealthCheckFinished {
            service_id: service_id.clone(),
            attempt,
            success,
            exit_code,
        })
        .await;
}

fn try_emit_spawn_failed(
    events_tx: &mpsc::Sender<Event>,
    service_id: &ServiceID,
    attempt: u64,
    source: &std::io::Error,
) {
    let _ = events_tx.try_send(Event::HealthCheckLogLine {
        service_id: service_id.clone(),
        attempt,
        stream: OutputStream::Stderr,
        line: source.to_string(),
    });
    let _ = events_tx.try_send(Event::HealthCheckFinished {
        service_id: service_id.clone(),
        attempt,
        success: false,
        exit_code: -1,
    });
}

fn spawn_output_task(
    mut lines: tokio::io::Lines<BufReader<impl tokio::io::AsyncRead + Unpin + Send + 'static>>,
    service_id: ServiceID,
    attempt: u64,
    stream: OutputStream,
    events_tx: mpsc::Sender<Event>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn(async move {
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let _ = events_tx
                        .send(Event::HealthCheckLogLine {
                            service_id: service_id.clone(),
                            attempt,
                            stream,
                            line,
                        })
                        .await;
                }
                Ok(None) => break,
                Err(err) => {
                    tracing::error!(service_id, ?err, "health check: failed to read line");
                }
            }
        }
    })
}

async fn join_tasks(tasks: &mut [Option<tokio::task::JoinHandle<()>>]) {
    for task in tasks {
        if let Some(task) = task.take() {
            let _ = task.await;
        }
    }
}

struct Running {
    service_id: ServiceID,
    attempt: u64,
    command: String,
    events_tx: mpsc::Sender<Event>,
    stdout_task: Option<tokio::task::JoinHandle<()>>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    wait_handle: tokio::task::JoinHandle<Result<std::process::ExitStatus, std::io::Error>>,
    kill_token: CancellationToken,
    child_pid: Option<i32>,
}

fn spawn_wait_task(
    mut child: tokio::process::Child,
    kill_token: &CancellationToken,
) -> tokio::task::JoinHandle<Result<std::process::ExitStatus, std::io::Error>> {
    let poll = std::time::Duration::from_millis(25);
    let kill_token_child = kill_token.clone();
    tokio::task::spawn(async move {
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {}
                Err(err) => return Err(err),
            }

            tokio::select! {
                () = kill_token_child.cancelled() => {
                    #[cfg(unix)]
                    if let Some(pid) = child.id() {
                        let pid = i32::try_from(pid).unwrap_or(i32::MAX);
                        let pid = nix::unistd::Pid::from_raw(pid);
                        let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
                    }
                    let _ = child.kill().await;
                    return child.wait().await;
                }
                () = tokio::time::sleep(poll) => {}
            }
        }
    })
}

async fn select_completion(
    timeout: Option<std::time::Duration>,
    shutdown: &CancellationToken,
    terminate: &CancellationToken,
    wait_handle: &mut tokio::task::JoinHandle<Result<std::process::ExitStatus, std::io::Error>>,
) -> Completion {
    tokio::select! {
        () = shutdown.cancelled() => Completion::Shutdown,
        () = terminate.cancelled() => Completion::Shutdown,
        () = async {
            if let Some(d) = timeout {
                tokio::time::sleep(d).await;
            } else {
                futures::future::pending::<()>().await;
            }
        } => Completion::Timeout,
        res = wait_handle => Completion::Status(res.unwrap_or_else(|err| Err(std::io::Error::other(err.to_string())))),
    }
}

async fn cleanup_after_cancel(running: &mut Running) {
    running.kill_token.cancel();
    #[cfg(unix)]
    if let Some(pid) = running.child_pid {
        kill_process_group(pid);
    }
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        &mut running.wait_handle,
    )
    .await;
    join_tasks(&mut [running.stdout_task.take(), running.stderr_task.take()]).await;
}

async fn finish_with_exit(running: &mut Running, success: bool, exit_code: i32) {
    join_tasks(&mut [running.stdout_task.take(), running.stderr_task.take()]).await;
    emit_finished(
        &running.events_tx,
        &running.service_id,
        running.attempt,
        success,
        exit_code,
    )
    .await;
}

#[cfg(unix)]
fn kill_process_group(pid: i32) {
    let pid = nix::unistd::Pid::from_raw(pid);
    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
}

async fn run(
    health_check: &crate::config::HealthCheck,
    service_id: &ServiceID,
    attempt: u64,
    params: RunParams<'_>,
) -> Result<(), Error> {
    let (prog, args) = &health_check.test;
    let command = command_string(health_check);

    emit_started(&params.events_tx, service_id, attempt, command.clone()).await;

    let mut cmd = Command::new(prog.as_ref());
    cmd.args(args.iter().map(std::convert::AsRef::as_ref))
        .envs(params.environment.iter())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped());
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    if let Some(dir) = params.working_dir {
        cmd.current_dir(dir);
    }

    let mut process = cmd.spawn().map_err(|source| {
        try_emit_spawn_failed(&params.events_tx, service_id, attempt, &source);
        Error {
            command: command.clone(),
            source: ErrorReason::Spawn(source),
        }
    })?;

    let mut stderr_task = None;
    if let Some(stderr) = process.stderr.take() {
        stderr_task = Some(spawn_output_task(
            BufReader::new(stderr).lines(),
            service_id.clone(),
            attempt,
            OutputStream::Stderr,
            params.events_tx.clone(),
        ));
    }

    let mut stdout_task = None;
    if let Some(stdout) = process.stdout.take() {
        stdout_task = Some(spawn_output_task(
            BufReader::new(stdout).lines(),
            service_id.clone(),
            attempt,
            OutputStream::Stdout,
            params.events_tx.clone(),
        ));
    }

    let child_pid = process.id().and_then(|pid| i32::try_from(pid).ok());
    let kill_token = CancellationToken::new();
    let mut wait_handle = spawn_wait_task(process, &kill_token);
    let timeout = health_check.timeout.as_ref().map(|t| t.inner);

    let completion =
        select_completion(timeout, &params.shutdown, &params.terminate, &mut wait_handle).await;

    let mut running = Running {
        service_id: service_id.clone(),
        attempt,
        command,
        events_tx: params.events_tx.clone(),
        stdout_task,
        stderr_task,
        wait_handle,
        kill_token,
        child_pid,
    };

    match completion {
        Completion::Shutdown => {
            cleanup_after_cancel(&mut running).await;
            emit_finished(&running.events_tx, &running.service_id, attempt, false, -1).await;
            Ok(())
        }
        Completion::Timeout => {
            cleanup_after_cancel(&mut running).await;
            emit_finished(&running.events_tx, &running.service_id, attempt, false, -1).await;
            let command = std::mem::take(&mut running.command);
            Err(Error {
                command,
                source: ErrorReason::Timeout,
            })
        }
        Completion::Status(Ok(status)) if status.success() => {
            finish_with_exit(&mut running, true, status.code().unwrap_or(0)).await;
            Ok(())
        }
        Completion::Status(Ok(status)) => {
            let exit_code = status.code().unwrap_or(-1);
            finish_with_exit(&mut running, false, exit_code).await;
            let command = std::mem::take(&mut running.command);
            Err(Error {
                command,
                source: ErrorReason::Failed { exit_code },
            })
        }
        Completion::Status(Err(err)) => {
            finish_with_exit(&mut running, false, -1).await;
            let command = std::mem::take(&mut running.command);
            Err(Error {
                command,
                source: ErrorReason::Spawn(err),
            })
        }
    }
}
