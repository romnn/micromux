use crate::{
    model::{HealthcheckConfig, RunSink},
    scheduler::{OutputStream, ProcessEvent, RunId, ServiceID},
};
use itertools::Itertools;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    strum::Display,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
)]
/// The resolved health verdict for a service.
pub enum Health {
    /// The service's healthcheck is currently passing.
    Healthy,
    /// The service's healthcheck is currently failing.
    Unhealthy,
    /// A newer peer sent a health verdict this binary does not know yet.
    #[serde(other)]
    Unknown,
}

/// How long to wait for stdout/stderr readers to flush after the probe process exits.
const OUTPUT_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::model::SessionModelReader;
    use crate::test_util::{initial_model_entry, spanned_string, unique_tmp_dir};
    use similar_asserts::assert_eq;
    use std::path::PathBuf;
    use yaml_spanned::Spanned;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(prefix: &str) -> color_eyre::eyre::Result<Self> {
            let dir = unique_tmp_dir(prefix);
            std::fs::create_dir_all(&dir)?;
            Ok(Self(dir))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn run_sink(id: &str, run_generation: u64) -> (SessionModelReader, RunSink) {
        let (reader, writer) = crate::model::new([initial_model_entry(id)]);
        let id = id.to_string();
        writer.begin_run(&id, run_generation);
        (reader, writer.run_sink(&id, run_generation))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_child_and_emits_finished() -> color_eyre::eyre::Result<()> {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        let dir = TempDir::new("micromux-hc-timeout")?;
        let pid_path = dir.0.join("pid");

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

        let (reader, sink) = run_sink("svc", 1);
        let shutdown = CancellationToken::new();
        let terminate = CancellationToken::new();
        let env = std::collections::HashMap::new();

        let start = tokio::time::Instant::now();
        let res = super::run(
            &hc,
            1,
            RunParams {
                working_dir: Some(dir.0.as_path()),
                environment: &env,
                sink,
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

        let history = reader.healthchecks("svc");
        let result = history
            .last()
            .and_then(|attempt| attempt.result.as_ref())
            .ok_or_else(|| color_eyre::eyre::eyre!("missing finished timeout attempt"))?;
        assert!(!result.success);
        assert!(!result.cancelled);

        match kill(Pid::from_raw(pid), None) {
            Err(Errno::ESRCH) => {}
            other => color_eyre::eyre::bail!("expected ESRCH for dead pid, got {other:?}"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn terminated_probe_emits_cancelled_finished() -> color_eyre::eyre::Result<()> {
        let hc = crate::config::HealthCheck {
            test: (
                spanned_string("sh"),
                vec![spanned_string("-c"), spanned_string("sleep 5")],
            ),
            start_delay: None,
            interval: None,
            timeout: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: std::time::Duration::from_secs(5),
            }),
            retries: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: 1,
            }),
        };

        let (reader, sink) = run_sink("svc", 1);
        let mut changes = reader.subscribe();
        let shutdown = CancellationToken::new();
        let terminate = CancellationToken::new();
        let run_handle = tokio::spawn({
            let terminate = terminate.clone();
            async move {
                let env = std::collections::HashMap::new();
                super::run(
                    &hc,
                    1,
                    RunParams {
                        working_dir: None,
                        environment: &env,
                        sink,
                        shutdown,
                        terminate,
                    },
                )
                .await
            }
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while reader.healthchecks("svc").is_empty() {
                let _ = changes.recv().await;
            }
        })
        .await
        .map_err(|_| color_eyre::eyre::eyre!("probe did not start"))?;
        terminate.cancel();

        let outcome = run_handle.await?;
        assert!(matches!(outcome, Ok(Outcome::Cancelled)));
        let history = reader.healthchecks("svc");
        let result = history
            .last()
            .and_then(|attempt| attempt.result.as_ref())
            .ok_or_else(|| color_eyre::eyre::eyre!("missing finished cancelled attempt"))?;
        assert!(result.cancelled);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_probe_does_not_wait_for_background_stdout_holder()
    -> color_eyre::eyre::Result<()> {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        let dir = TempDir::new("micromux-hc-bg-stdout")?;
        let pid_path = dir.0.join("pid");

        let hc = crate::config::HealthCheck {
            test: (
                spanned_string("sh"),
                vec![
                    spanned_string("-c"),
                    spanned_string(&format!(
                        "(trap '' TERM HUP; sleep 5) & echo $! > {}; echo healthy; exit 0",
                        pid_path.to_string_lossy()
                    )),
                ],
            ),
            start_delay: None,
            interval: None,
            timeout: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: std::time::Duration::from_secs(5),
            }),
            retries: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: 1,
            }),
        };

        let (_reader, sink) = run_sink("svc", 1);
        let shutdown = CancellationToken::new();
        let terminate = CancellationToken::new();
        let env = std::collections::HashMap::new();

        let res = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::run(
                &hc,
                1,
                RunParams {
                    working_dir: Some(dir.0.as_path()),
                    environment: &env,
                    sink,
                    shutdown,
                    terminate,
                },
            ),
        )
        .await
        .map_err(|_| {
            color_eyre::eyre::eyre!("healthcheck waited for a background stdout holder")
        })?;

        assert!(matches!(res, Ok(Outcome::Healthy)));

        let pid_str = std::fs::read_to_string(&pid_path)?;
        let pid: i32 = pid_str.trim().parse()?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match kill(Pid::from_raw(pid), None) {
                Err(Errno::ESRCH) => break,
                other if tokio::time::Instant::now() < deadline => {
                    if !matches!(other, Ok(())) {
                        color_eyre::eyre::bail!(
                            "unexpected signal probe result for background process: {other:?}"
                        );
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                other => {
                    color_eyre::eyre::bail!(
                        "expected background healthcheck process to be gone, got {other:?}"
                    );
                }
            }
        }

        Ok(())
    }

    async fn started_attempts_before_unhealthy(retries: usize) -> color_eyre::eyre::Result<usize> {
        let hc = crate::config::HealthCheck {
            test: (
                spanned_string("sh"),
                vec![spanned_string("-c"), spanned_string("exit 1")],
            ),
            start_delay: None,
            interval: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: std::time::Duration::from_secs(10),
            }),
            timeout: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: std::time::Duration::from_millis(500),
            }),
            retries: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: retries,
            }),
        };

        let (events_tx, mut events_rx) = mpsc::channel(64);
        let shutdown = CancellationToken::new();
        let terminate = CancellationToken::new();
        let service_id: ServiceID = "svc".to_string();
        let run_id = RunId::new(1);
        let (reader, sink) = run_sink(&service_id, run_id.get());

        let handle = tokio::spawn({
            let terminate = terminate.clone();
            async move {
                run_loop(
                    hc,
                    RunLoopParams {
                        service_id,
                        run_id,
                        sink,
                        working_dir: None,
                        environment: std::collections::HashMap::new(),
                        events_tx,
                        shutdown,
                        terminate,
                    },
                )
                .await;
            }
        });

        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv())
                .await?
                .ok_or_else(|| color_eyre::eyre::eyre!("healthcheck event channel closed"))?;
            if let ProcessEvent::Unhealthy { .. } = event {
                break;
            }
        }

        terminate.cancel();
        handle.await?;
        Ok(reader.healthchecks("svc").len())
    }

    #[tokio::test]
    async fn retries_is_number_of_failures_before_unhealthy() -> color_eyre::eyre::Result<()> {
        assert_eq!(started_attempts_before_unhealthy(1).await?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn zero_retries_is_floored_to_one_failure() -> color_eyre::eyre::Result<()> {
        assert_eq!(started_attempts_before_unhealthy(0).await?, 1);
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
    pub sink: RunSink,
    pub shutdown: CancellationToken,
    pub terminate: CancellationToken,
}

pub(crate) struct RunLoopParams {
    pub service_id: ServiceID,
    pub run_id: RunId,
    pub sink: RunSink,
    pub working_dir: Option<std::path::PathBuf>,
    pub environment: std::collections::HashMap<String, String>,
    pub events_tx: mpsc::Sender<ProcessEvent>,
    pub shutdown: CancellationToken,
    pub terminate: CancellationToken,
}

enum Completion {
    Shutdown,
    Timeout,
    Status(Result<std::process::ExitStatus, std::io::Error>),
}

/// The result of a single successful (or cancelled) probe run.
enum Outcome {
    /// The probe completed successfully.
    Healthy,
    /// The probe was cancelled because the service is shutting down or restarting.
    Cancelled,
}

fn log_probe_error(
    source: &ErrorReason,
    service_id: &ServiceID,
    attempt: usize,
    max_retries: usize,
) {
    match source {
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
}

async fn record_probe_failure(
    params: &RunLoopParams,
    source: &ErrorReason,
    attempt: &mut usize,
    max_retries: usize,
    unhealthy: &mut bool,
) {
    log_probe_error(source, &params.service_id, *attempt, max_retries);

    // Mark unhealthy once on the failing transition, then keep probing so the service can
    // recover back to healthy (and unblock `condition: healthy` dependents) instead of giving
    // up permanently.
    if *unhealthy {
        return;
    }

    *attempt = attempt.saturating_add(1);
    if *attempt >= max_retries {
        *unhealthy = true;
        let _ = params
            .events_tx
            .send(ProcessEvent::Unhealthy {
                service_id: params.service_id.clone(),
                run_id: params.run_id,
            })
            .await;
    }
}

pub async fn run_loop(health_check: crate::config::HealthCheck, params: RunLoopParams) {
    let effective = HealthcheckConfig::from(&health_check);
    let max_retries = effective.retries;
    let start_delay = health_check
        .start_delay
        .as_deref()
        .copied()
        .unwrap_or_default();
    let interval = effective.interval;
    tracing::info!(
        service_id = params.service_id,
        ?start_delay,
        ?interval,
        max_retries,
        "starting health check loop"
    );

    if !start_delay.is_zero() {
        tokio::select! {
            () = params.shutdown.cancelled() => return,
            () = params.terminate.cancelled() => return,
            () = tokio::time::sleep(start_delay) => {},
        };
    }

    let mut attempt = 0;
    let mut unhealthy = false;
    let mut attempt_id: u64 = 0;
    loop {
        attempt_id = attempt_id.wrapping_add(1);

        let res = run(
            &health_check,
            attempt_id,
            RunParams {
                working_dir: params.working_dir.as_deref(),
                environment: &params.environment,
                sink: params.sink.clone(),
                shutdown: params.shutdown.clone(),
                terminate: params.terminate.clone(),
            },
        )
        .await;
        match res {
            Ok(Outcome::Cancelled) => return,
            Ok(Outcome::Healthy) => {
                let _ = params
                    .events_tx
                    .send(ProcessEvent::Healthy {
                        service_id: params.service_id.clone(),
                        run_id: params.run_id,
                    })
                    .await;
                attempt = 0;
                unhealthy = false;
            }
            Err(err) => {
                record_probe_failure(
                    &params,
                    &err.source,
                    &mut attempt,
                    max_retries,
                    &mut unhealthy,
                )
                .await;
            }
        }

        tokio::select! {
            () = params.shutdown.cancelled() => return,
            () = params.terminate.cancelled() => return,
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

fn emit_spawn_failed(sink: &RunSink, attempt: u64, source: &std::io::Error) {
    sink.append_health_line(attempt, OutputStream::Stderr, source.to_string());
    sink.finish_health_attempt(attempt, false, -1, false);
}

fn spawn_output_task(
    mut lines: tokio::io::Lines<BufReader<impl tokio::io::AsyncRead + Unpin + Send + 'static>>,
    attempt: u64,
    stream: OutputStream,
    sink: RunSink,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn(async move {
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    sink.append_health_line(attempt, stream, line);
                }
                Ok(None) => break,
                Err(err) => {
                    tracing::error!(?err, "health check: failed to read line");
                }
            }
        }
    })
}

#[derive(Default)]
struct OutputReaders {
    stdout: Option<tokio::task::JoinHandle<()>>,
    stderr: Option<tokio::task::JoinHandle<()>>,
}

impl OutputReaders {
    fn set_stdout(&mut self, task: tokio::task::JoinHandle<()>) {
        self.stdout = Some(task);
    }

    fn set_stderr(&mut self, task: tokio::task::JoinHandle<()>) {
        self.stderr = Some(task);
    }

    async fn drain(&mut self, timeout: std::time::Duration) -> bool {
        let (stdout_drained, stderr_drained) = tokio::join!(
            Self::join_with_timeout(&mut self.stdout, timeout),
            Self::join_with_timeout(&mut self.stderr, timeout),
        );
        stdout_drained && stderr_drained
    }

    async fn abort_and_join(&mut self) {
        Self::abort_task(&mut self.stdout).await;
        Self::abort_task(&mut self.stderr).await;
    }

    async fn join_with_timeout(
        task: &mut Option<tokio::task::JoinHandle<()>>,
        timeout: std::time::Duration,
    ) -> bool {
        let Some(mut handle) = task.take() else {
            return true;
        };

        match tokio::time::timeout(timeout, &mut handle).await {
            Ok(Ok(())) => true,
            Ok(Err(err)) => {
                tracing::error!(?err, "health check output reader task failed");
                true
            }
            Err(_) => {
                *task = Some(handle);
                false
            }
        }
    }

    async fn abort_task(task: &mut Option<tokio::task::JoinHandle<()>>) {
        if let Some(task) = task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for OutputReaders {
    fn drop(&mut self) {
        if let Some(task) = self.stdout.take() {
            task.abort();
        }
        if let Some(task) = self.stderr.take() {
            task.abort();
        }
    }
}

struct Running {
    sink: RunSink,
    attempt: u64,
    command: String,
    output_readers: OutputReaders,
    wait_handle: tokio::task::JoinHandle<Result<std::process::ExitStatus, std::io::Error>>,
    kill_token: CancellationToken,
    #[cfg(unix)]
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
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), &mut running.wait_handle).await;
    running.output_readers.abort_and_join().await;
}

async fn finish_with_exit(running: &mut Running, success: bool, exit_code: i32) {
    let drained = running.output_readers.drain(OUTPUT_DRAIN_TIMEOUT).await;

    #[cfg(unix)]
    if !drained && let Some(pid) = running.child_pid {
        kill_reaped_probe_group(pid);
        running.output_readers.abort_and_join().await;
    }

    #[cfg(not(unix))]
    if !drained {
        // Windows has no process-group kill path here; still abort the reader tasks so a
        // descendant that inherited the pipe cannot block the healthcheck loop.
        running.output_readers.abort_and_join().await;
    }

    running
        .sink
        .finish_health_attempt(running.attempt, success, exit_code, false);
}

#[cfg(unix)]
fn kill_process_group(pid: i32) {
    let pid = nix::unistd::Pid::from_raw(pid);
    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
}

#[cfg(unix)]
fn kill_reaped_probe_group(pid: i32) {
    let pid = nix::unistd::Pid::from_raw(pid);
    let _ = nix::sys::signal::killpg(pid, nix::sys::signal::Signal::SIGKILL);
}

#[expect(
    clippy::too_many_lines,
    reason = "the probe lifecycle keeps spawn, timeout, output drain, and cleanup ordering together"
)]
async fn run(
    health_check: &crate::config::HealthCheck,
    attempt: u64,
    params: RunParams<'_>,
) -> Result<Outcome, Error> {
    let (prog, args) = &health_check.test;
    let command = command_string(health_check);

    params.sink.start_health_attempt(attempt, command.clone());

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
        emit_spawn_failed(&params.sink, attempt, &source);
        Error {
            command: command.clone(),
            source: ErrorReason::Spawn(source),
        }
    })?;

    let mut output_readers = OutputReaders::default();
    if let Some(stderr) = process.stderr.take() {
        output_readers.set_stderr(spawn_output_task(
            BufReader::new(stderr).lines(),
            attempt,
            OutputStream::Stderr,
            params.sink.clone(),
        ));
    }

    if let Some(stdout) = process.stdout.take() {
        output_readers.set_stdout(spawn_output_task(
            BufReader::new(stdout).lines(),
            attempt,
            OutputStream::Stdout,
            params.sink.clone(),
        ));
    }

    #[cfg(unix)]
    let child_pid = process.id().and_then(|pid| i32::try_from(pid).ok());
    let kill_token = CancellationToken::new();
    let mut wait_handle = spawn_wait_task(process, &kill_token);
    // Always bound the probe so a hung command cannot block the loop (and dependents) forever.
    let timeout = Some(HealthcheckConfig::from(health_check).timeout);

    let completion = select_completion(
        timeout,
        &params.shutdown,
        &params.terminate,
        &mut wait_handle,
    )
    .await;

    let mut running = Running {
        sink: params.sink,
        attempt,
        command,
        output_readers,
        wait_handle,
        kill_token,
        #[cfg(unix)]
        child_pid,
    };

    match completion {
        Completion::Shutdown => {
            // The service is being stopped/restarted: this is a cancellation, not a probe
            // result. Tear down the probe but do NOT report it as healthy or unhealthy.
            cleanup_after_cancel(&mut running).await;
            running.sink.finish_health_attempt(attempt, false, -1, true);
            Ok(Outcome::Cancelled)
        }
        Completion::Timeout => {
            cleanup_after_cancel(&mut running).await;
            running
                .sink
                .finish_health_attempt(attempt, false, -1, false);
            let command = std::mem::take(&mut running.command);
            Err(Error {
                command,
                source: ErrorReason::Timeout,
            })
        }
        Completion::Status(Ok(status)) if status.success() => {
            finish_with_exit(&mut running, true, status.code().unwrap_or(0)).await;
            Ok(Outcome::Healthy)
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
