use crate::{
    model::RunSink,
    scheduler::{OutputStream, ProcessEvent, RunId, ServiceID},
};
use itertools::Itertools;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
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
    use color_eyre::eyre;
    use similar_asserts::assert_eq;
    use std::path::PathBuf;
    use yaml_spanned::Spanned;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(prefix: &str) -> eyre::Result<Self> {
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
    async fn timeout_kills_child_and_emits_finished() -> eyre::Result<()> {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        let dir = TempDir::new("micromux-hc-timeout")?;
        let pid_path = dir.0.join("pid");

        let hc: crate::HealthcheckSpec = crate::config::HealthCheck {
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
        }
        .into();

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
            .ok_or_else(|| eyre::eyre!("missing finished timeout attempt"))?;
        assert!(!result.success);
        assert!(!result.cancelled);

        match kill(Pid::from_raw(pid), None) {
            Err(Errno::ESRCH) => {}
            other => {
                eyre::bail!("expected ESRCH for dead pid, got {other:?}");
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn terminated_probe_emits_cancelled_finished() -> eyre::Result<()> {
        let hc: crate::HealthcheckSpec = crate::config::HealthCheck {
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
        }
        .into();

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
        .map_err(|_| eyre::eyre!("probe did not start"))?;
        terminate.cancel();

        let outcome = run_handle.await?;
        assert!(matches!(outcome, Ok(Outcome::Cancelled)));
        let history = reader.healthchecks("svc");
        let result = history
            .last()
            .and_then(|attempt| attempt.result.as_ref())
            .ok_or_else(|| eyre::eyre!("missing finished cancelled attempt"))?;
        assert!(result.cancelled);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_probe_kills_its_background_process_group_before_reaping() -> eyre::Result<()>
    {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        let dir = TempDir::new("micromux-hc-bg-stdout")?;
        let pid_path = dir.0.join("pid");

        let hc: crate::HealthcheckSpec = crate::config::HealthCheck {
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
        }
        .into();

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
        .map_err(|_| eyre::eyre!("healthcheck waited for a background stdout holder"))?;

        assert!(matches!(res, Ok(Outcome::Healthy)));

        let pid_str = std::fs::read_to_string(&pid_path)?;
        let pid: i32 = pid_str.trim().parse()?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match kill(Pid::from_raw(pid), None) {
                Err(Errno::ESRCH) => break,
                Ok(()) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                other => {
                    eyre::bail!("background probe process survived group cleanup: {other:?}");
                }
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn probe_output_lines_are_bounded_before_model_ingress() -> eyre::Result<()> {
        use tokio::io::AsyncWriteExt as _;

        let (reader, mut writer) = tokio::io::duplex(crate::model::MODEL_STRING_MAX_BYTES * 2);
        let write = tokio::spawn(async move {
            writer
                .write_all(&vec![b'x'; crate::model::MODEL_STRING_MAX_BYTES * 2])
                .await?;
            writer.write_all(b"\nnext\n").await
        });
        let mut reader = BufReader::new(reader);
        let mut buffer = Vec::new();

        let first = read_bounded_line(&mut reader, &mut buffer)
            .await?
            .ok_or_else(|| eyre::eyre!("missing oversized line"))?;
        let second = read_bounded_line(&mut reader, &mut buffer)
            .await?
            .ok_or_else(|| eyre::eyre!("missing following line"))?;
        write.await??;

        assert_eq!(first.len(), crate::model::MODEL_STRING_MAX_BYTES);
        assert_eq!(second, "next");
        Ok(())
    }

    #[tokio::test]
    async fn probe_output_limit_preserves_utf8_boundaries() -> eyre::Result<()> {
        use tokio::io::AsyncWriteExt as _;

        let (reader, mut writer) = tokio::io::duplex(crate::model::MODEL_STRING_MAX_BYTES * 2);
        let write = tokio::spawn(async move {
            writer
                .write_all(&vec![b'x'; crate::model::MODEL_STRING_MAX_BYTES - 1])
                .await?;
            writer.write_all("é trailing\n".as_bytes()).await
        });
        let mut reader = BufReader::new(reader);
        let mut buffer = Vec::new();

        let line = read_bounded_line(&mut reader, &mut buffer)
            .await?
            .ok_or_else(|| eyre::eyre!("missing oversized UTF-8 line"))?;
        write.await??;

        assert_eq!(line.len(), crate::model::MODEL_STRING_MAX_BYTES - 1);
        assert!(!line.contains('\u{fffd}'));
        Ok(())
    }

    async fn started_attempts_before_unhealthy(retries: usize) -> eyre::Result<usize> {
        let hc: crate::HealthcheckSpec = crate::config::HealthCheck {
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
        }
        .into();

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
                .ok_or_else(|| eyre::eyre!("healthcheck event channel closed"))?;
            if let ProcessEvent::Unhealthy { .. } = event {
                break;
            }
        }

        terminate.cancel();
        handle.await?;
        Ok(reader.healthchecks("svc").len())
    }

    #[tokio::test]
    async fn retries_is_number_of_failures_before_unhealthy() -> eyre::Result<()> {
        assert_eq!(started_attempts_before_unhealthy(1).await?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn zero_retries_is_floored_to_one_failure() -> eyre::Result<()> {
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
    pub working_dir: Option<crate::service::SpawnWorkingDirectory>,
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
) -> bool {
    log_probe_error(source, &params.service_id, *attempt, max_retries);

    // Mark unhealthy once on the failing transition, then keep probing so the service can
    // recover back to healthy (and unblock `condition: healthy` dependents) instead of giving
    // up permanently.
    if *unhealthy {
        return true;
    }

    *attempt = attempt.saturating_add(1);
    if *attempt >= max_retries {
        *unhealthy = true;
        return send_event(
            params,
            ProcessEvent::Unhealthy {
                service_id: params.service_id.clone(),
                run_id: params.run_id,
            },
        )
        .await;
    }
    true
}

async fn send_event(params: &RunLoopParams, event: ProcessEvent) -> bool {
    tokio::select! {
        biased;
        () = params.shutdown.cancelled() => false,
        () = params.terminate.cancelled() => false,
        result = params.events_tx.send(event) => result.is_ok(),
    }
}

pub async fn run_loop(health_check: crate::HealthcheckSpec, params: RunLoopParams) {
    let max_retries = health_check.retries;
    let start_delay = health_check.start_delay.unwrap_or_default();
    let interval = health_check.interval;
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
    // Run one probe at a time so a slow command cannot accumulate overlapping descendants.
    // `HealthcheckSpec::retries` documents the resulting worst-case transition latency.
    loop {
        attempt_id = attempt_id.wrapping_add(1);

        let res = run(
            &health_check,
            attempt_id,
            RunParams {
                working_dir: params
                    .working_dir
                    .as_ref()
                    .map(crate::service::SpawnWorkingDirectory::as_path),
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
                if !send_event(
                    &params,
                    ProcessEvent::Healthy {
                        service_id: params.service_id.clone(),
                        run_id: params.run_id,
                    },
                )
                .await
                {
                    return;
                }
                attempt = 0;
                unhealthy = false;
            }
            Err(err) => {
                if !record_probe_failure(
                    &params,
                    &err.source,
                    &mut attempt,
                    max_retries,
                    &mut unhealthy,
                )
                .await
                {
                    return;
                }
            }
        }

        tokio::select! {
            () = params.shutdown.cancelled() => return,
            () = params.terminate.cancelled() => return,
            () = tokio::time::sleep(interval) => {},
        };
    }
}

fn command_string(health_check: &crate::HealthcheckSpec) -> String {
    health_check.test.iter().join(" ")
}

fn emit_spawn_failed(sink: &RunSink, attempt: u64, source: &std::io::Error) {
    sink.append_health_line(attempt, OutputStream::Stderr, source.to_string());
    sink.finish_health_attempt(attempt, false, -1, false);
}

fn spawn_output_task(
    reader: impl AsyncRead + Unpin + Send + 'static,
    attempt: u64,
    stream: OutputStream,
    sink: RunSink,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn(async move {
        let mut reader = BufReader::new(reader);
        let mut line = Vec::new();
        loop {
            match read_bounded_line(&mut reader, &mut line).await {
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

async fn read_bounded_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    line: &mut Vec<u8>,
) -> std::io::Result<Option<String>> {
    line.clear();
    let mut saw_input = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if !saw_input {
                return Ok(None);
            }
            break;
        }
        saw_input = true;
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |idx| idx + 1);
        let capture_limit = crate::model::MODEL_STRING_MAX_BYTES.saturating_add(3);
        let remaining = capture_limit.saturating_sub(line.len());
        if let Some(chunk) = available.get(..end.min(remaining)) {
            line.extend_from_slice(chunk);
        }
        let finished = available.get(end.saturating_sub(1)) == Some(&b'\n');
        reader.consume(end);
        if finished {
            break;
        }
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(Some(crate::model::truncate_to_first_bytes(
        String::from_utf8_lossy(line).into_owned(),
        crate::model::MODEL_STRING_MAX_BYTES,
    )))
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
    process: tokio::process::Child,
    #[cfg(windows)]
    process_job: Option<win32job::Job>,
}

#[cfg(unix)]
fn child_exit_is_pending(pid: i32) -> std::io::Result<bool> {
    let pid = rustix::process::Pid::from_raw(pid)
        .ok_or_else(|| std::io::Error::other("healthcheck pid must be positive"))?;
    loop {
        match rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOWAIT
                | rustix::process::WaitIdOptions::NOHANG,
        ) {
            Ok(status) => return Ok(status.is_some()),
            Err(rustix::io::Errno::INTR) => {}
            Err(err) => return Err(err.into()),
        }
    }
}

#[cfg(unix)]
async fn wait_for_exit_without_reaping(pid: i32) -> std::io::Result<()> {
    let mut child_signal =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()).ok();
    loop {
        if child_exit_is_pending(pid)? {
            return Ok(());
        }
        if let Some(signal) = child_signal.as_mut() {
            if signal.recv().await.is_none() {
                child_signal = None;
            }
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

#[cfg(unix)]
async fn wait_for_probe_status(
    process: &mut tokio::process::Child,
) -> Result<std::process::ExitStatus, std::io::Error> {
    let Some(pid) = process.id().and_then(|pid| i32::try_from(pid).ok()) else {
        return process.wait().await;
    };
    wait_for_exit_without_reaping(pid).await?;
    kill_process_group(pid);
    process.wait().await
}

#[cfg(not(unix))]
async fn wait_for_probe_status(
    process: &mut tokio::process::Child,
) -> Result<std::process::ExitStatus, std::io::Error> {
    process.wait().await
}

async fn select_completion(
    timeout: Option<std::time::Duration>,
    shutdown: &CancellationToken,
    terminate: &CancellationToken,
    process: &mut tokio::process::Child,
) -> Completion {
    tokio::select! {
        biased;
        status = wait_for_probe_status(process) => Completion::Status(status),
        () = shutdown.cancelled() => Completion::Shutdown,
        () = terminate.cancelled() => Completion::Shutdown,
        () = async {
            if let Some(d) = timeout {
                tokio::time::sleep(d).await;
            } else {
                futures::future::pending::<()>().await;
            }
        } => Completion::Timeout,
    }
}

async fn cleanup_after_cancel(running: &mut Running) {
    #[cfg(unix)]
    if let Some(pid) = running.process.id().and_then(|pid| i32::try_from(pid).ok()) {
        // The owned child has not been waited yet, so its PID and process-group id cannot be
        // recycled before this signal.
        kill_process_group(pid);
    }
    #[cfg(windows)]
    drop(running.process_job.take());
    let already_exited = running.process.try_wait().ok().flatten().is_some();
    if !already_exited {
        let _ = running.process.start_kill();
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(1), running.process.wait()).await;
    }
    if !running.output_readers.drain(OUTPUT_DRAIN_TIMEOUT).await {
        running.output_readers.abort_and_join().await;
    }
}

async fn finish_with_exit(running: &mut Running, success: bool, exit_code: i32) {
    #[cfg(windows)]
    drop(running.process_job.take());
    let drained = running.output_readers.drain(OUTPUT_DRAIN_TIMEOUT).await;

    if !drained {
        // A descendant may create another process group while retaining an output pipe. Abort the
        // readers after the owned group is gone instead of signaling a post-reap numeric id.
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

#[expect(
    clippy::too_many_lines,
    reason = "the probe lifecycle keeps spawn, timeout, output drain, and cleanup ordering together"
)]
async fn run(
    health_check: &crate::HealthcheckSpec,
    attempt: u64,
    params: RunParams<'_>,
) -> Result<Outcome, Error> {
    let command = command_string(health_check);

    let Some((prog, args)) = health_check.test.split_first() else {
        let source = std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "healthcheck command is empty",
        );
        emit_spawn_failed(&params.sink, attempt, &source);
        return Err(Error {
            command,
            source: ErrorReason::Spawn(source),
        });
    };

    params.sink.start_health_attempt(attempt, command.clone());

    let mut cmd = Command::new(prog);
    cmd.args(args)
        .envs(params.environment.iter())
        .kill_on_drop(true)
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
    #[cfg(windows)]
    let process_job = {
        let handle = process.raw_handle().ok_or_else(|| {
            let source = std::io::Error::other("healthcheck child exposed no process handle");
            emit_spawn_failed(&params.sink, attempt, &source);
            Error {
                command: command.clone(),
                source: ErrorReason::Spawn(source),
            }
        })?;
        Some(
            crate::windows_job::attach_kill_on_close(handle as isize).map_err(|message| {
                let source = std::io::Error::other(message);
                emit_spawn_failed(&params.sink, attempt, &source);
                Error {
                    command: command.clone(),
                    source: ErrorReason::Spawn(source),
                }
            })?,
        )
    };

    let mut output_readers = OutputReaders::default();
    if let Some(stderr) = process.stderr.take() {
        output_readers.set_stderr(spawn_output_task(
            stderr,
            attempt,
            OutputStream::Stderr,
            params.sink.clone(),
        ));
    }

    if let Some(stdout) = process.stdout.take() {
        output_readers.set_stdout(spawn_output_task(
            stdout,
            attempt,
            OutputStream::Stdout,
            params.sink.clone(),
        ));
    }

    // Always bound the probe so a hung command cannot block the loop (and dependents) forever.
    let timeout = Some(health_check.timeout);

    let completion =
        select_completion(timeout, &params.shutdown, &params.terminate, &mut process).await;

    let mut running = Running {
        sink: params.sink,
        attempt,
        command,
        output_readers,
        process,
        #[cfg(windows)]
        process_job,
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
