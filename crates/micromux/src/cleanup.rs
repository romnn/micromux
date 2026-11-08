//! Running a service's `cleanup` command after each of its processes exits.
//!
//! The supervisor runs a run's cleanup from its termination task, after the process exited and
//! before the run is reported as exited, so a restart or a shutdown waits for it.
//! Under containment it hands the command to a cleanup-only shim, which keeps running when the
//! supervisor dies; without containment it runs the command itself.
//! A service shim whose supervisor died runs the cleanup on its own (see [`crate::containment`]).
//!
//! Each service with a cleanup keeps two files in the user's state directory:
//!
//! - `<service>.log` holds the output of the latest cleanup, framed by a header and a footer
//!   line; the supervisor also mirrors it into the run's log as `[cleanup]` lines.
//! - `<service>.lock` is held exclusively while a cleanup runs under containment, so a restarted
//!   session can wait for a cleanup the previous session left to its shims.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt as _;
use tokio_util::sync::CancellationToken;

use crate::model::RunSink;
use crate::scheduler::{LogUpdateKind, OutputStream, ServiceID};
use crate::service::SpawnWorkingDirectory;
use crate::spec::{CleanupFiles, CleanupSpec, StopSignal};

/// How long a timed-out cleanup's processes get between the stop signal and `SIGKILL`.
pub(crate) const CLEANUP_STOP_GRACE: Duration = Duration::from_secs(5);

/// Exit status of a cleanup-only shim whose cleanup ran past its timeout, as `timeout(1)`
/// reports one.
pub(crate) const EXIT_TIMED_OUT: i32 = 124;

/// How often the supervisor mirrors new cleanup output into the run's log.
const TAIL_INTERVAL: Duration = Duration::from_millis(200);

/// Slack the supervisor allows a cleanup-only shim beyond its own timeout and stop grace before
/// it kills the shim's process group.
const SHIM_SLACK: Duration = Duration::from_secs(5);

/// How long a killed cleanup-only shim may take to be reaped.
const KILL_WAIT: Duration = Duration::from_secs(1);

/// How often a starting session checks whether a previous session's cleanup released its lock.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Longest partial line kept while waiting for its newline.
const LINE_MAX_BYTES: usize = 16 * 1024;

/// The byte that tells a cleanup-only shim to start its cleanup.
const GO: u8 = b'\n';

/// Where the cleanup files of a session's services live.
///
/// `config_path` is the canonical path of the session's config file, or its config directory
/// when it was not loaded from a file.
/// Returns `None` when the user has no state directory.
#[must_use]
pub fn cleanup_state_directory(config_path: &Path) -> Option<PathBuf> {
    crate::session::user_state_directory().map(|state| cleanup_directory_in(&state, config_path))
}

/// Where the cleanup files of a session's services live inside `state_directory`, the directory
/// [`crate::STATE_DIR_VARIABLE`] names for an isolated session.
#[must_use]
pub fn cleanup_directory_in(state_directory: &Path, config_path: &Path) -> PathBuf {
    state_directory
        .join("cleanup")
        .join(crate::session::config_path_key(config_path))
}

/// The path that keys a config's cleanup files.
pub(crate) fn config_key_path<F>(config_file: &crate::ConfigFile<F>) -> &Path {
    config_file
        .config_path
        .as_deref()
        .unwrap_or(&config_file.config_dir)
}

/// The cleanup files of `service_id` in `directory`.
///
/// Service ids are valid file names, so they need no escaping.
pub(crate) fn files_in(directory: &Path, service_id: &str) -> CleanupFiles {
    CleanupFiles {
        log: directory.join(format!("{service_id}.log")),
        lock: directory.join(format!("{service_id}.lock")),
    }
}

/// Opens a cleanup lock file, creating it and its private directory when missing.
#[cfg(any(unix, test))]
fn open_lock(path: &Path) -> std::io::Result<File> {
    if let Some(directory) = path.parent() {
        crate::session::create_private_dir(directory)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Takes the cleanup lock at `path` if nobody holds it.
///
/// Returns `None` when it is held, or cannot be opened.
#[cfg(unix)]
pub(crate) fn try_lock(path: &Path) -> Option<File> {
    let file = open_lock(path).ok()?;
    file.try_lock().ok().map(|()| file)
}

/// Takes the cleanup lock at `path`, waiting for its holder to release it.
///
/// # Errors
///
/// Returns an error when the lock file cannot be opened or locked.
#[cfg(unix)]
pub(crate) fn lock(path: &Path) -> std::io::Result<File> {
    let file = open_lock(path)?;
    file.lock()?;
    Ok(file)
}

/// Opens a cleanup log for writing, creating it and its private directory when missing.
///
/// With `truncate` the previous cleanup's output is dropped; otherwise writes append.
///
/// # Errors
///
/// Returns an error when the directory or the file cannot be created or opened.
pub(crate) fn open_log(path: &Path, truncate: bool) -> std::io::Result<File> {
    if let Some(directory) = path.parent() {
        crate::session::create_private_dir(directory)?;
    }
    let mut options = OpenOptions::new();
    options.create(true);
    if truncate {
        options.write(true).truncate(true);
    } else {
        options.append(true);
    }
    options.open(path)
}

/// The line that opens a cleanup's log: which run it follows, when it started, and what it runs.
///
/// `run_generation` is `None` for a cleanup a service shim runs after the supervisor died.
pub(crate) fn header(run_generation: Option<u64>, command: &[impl AsRef<OsStr>]) -> String {
    let after = run_generation.map_or_else(
        || "after micromux exited".to_string(),
        |generation| format!("after run {generation}"),
    );
    format!(
        "micromux: cleanup {after}, started {}: {}",
        humantime::format_rfc3339_seconds(std::time::SystemTime::now()),
        crate::shell_join(command)
    )
}

/// How a cleanup command ended, for the footer of its log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ending {
    /// It exited with this status.
    Exited(i32),
    /// A signal killed it.
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "only Unix processes die of signals")
    )]
    Signaled(String),
    /// It ran past this timeout and was stopped.
    TimedOut(Duration),
    /// It could not be started.
    CannotRun(String),
}

/// The line that closes a cleanup's log.
pub(crate) fn footer(ending: &Ending) -> String {
    match ending {
        Ending::Exited(code) => format!("micromux: cleanup exited with {code}"),
        Ending::Signaled(signal) => format!("micromux: cleanup killed by {signal}"),
        Ending::TimedOut(timeout) => format!(
            "micromux: cleanup timed out after {}",
            humantime::format_duration(*timeout)
        ),
        Ending::CannotRun(error) => format!("micromux: cannot run the cleanup: {error}"),
    }
}

/// Appends `line` to the cleanup log at `path`, which a failure leaves without it.
fn append_line(path: &Path, line: &str) {
    if let Err(err) = open_log(path, false).and_then(|mut log| writeln!(log, "{line}")) {
        tracing::warn!(?err, path = %path.display(), "cannot write the cleanup log");
    }
}

/// [`wait_for_previous_cleanups`] stopped waiting because it was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("cancelled while waiting for a previous session's cleanup")]
pub struct CleanupWaitCancelled;

/// Waits until no earlier session of `config_file` is still running a cleanup.
///
/// A session whose supervisor died leaves its cleanups to the service shims, and a session
/// started on the same config must not start services while those cleanups tear them down.
/// Every cleanup lock of the config counts, including those of services it no longer has.
/// `on_wait` is called once, with the services whose cleanups are still running, if there is
/// anything to wait for.
///
/// # Errors
///
/// Returns [`CleanupWaitCancelled`] when `cancel` is cancelled before every lock is free.
pub async fn wait_for_previous_cleanups<F>(
    config_file: &crate::ConfigFile<F>,
    cancel: &CancellationToken,
    on_wait: impl FnOnce(&[ServiceID]),
) -> Result<(), CleanupWaitCancelled> {
    let Some(directory) = cleanup_state_directory(config_key_path(config_file)) else {
        return Ok(());
    };
    wait_for_locks(&directory, cancel, on_wait).await
}

/// Waits until every `*.lock` file in `directory` can be locked.
async fn wait_for_locks(
    directory: &Path,
    cancel: &CancellationToken,
    on_wait: impl FnOnce(&[ServiceID]),
) -> Result<(), CleanupWaitCancelled> {
    let mut on_wait = Some(on_wait);
    loop {
        let busy = busy_locks(directory);
        if busy.is_empty() {
            return Ok(());
        }
        if let Some(on_wait) = on_wait.take() {
            on_wait(&busy);
        }
        tokio::select! {
            () = cancel.cancelled() => return Err(CleanupWaitCancelled),
            () = tokio::time::sleep(LOCK_POLL_INTERVAL) => {}
        }
    }
}

/// The services whose cleanup lock in `directory` is held, sorted.
fn busy_locks(directory: &Path) -> Vec<ServiceID> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut busy = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension() != Some(OsStr::new("lock")) {
                return None;
            }
            let service = path.file_stem()?.to_str()?.to_owned();
            let file = File::open(&path).ok()?;
            // A probe that takes the lock releases it when the file closes.
            match file.try_lock() {
                Err(TryLockError::WouldBlock) => Some(service),
                Ok(()) | Err(TryLockError::Error(_)) => None,
            }
        })
        .collect::<Vec<_>>();
    busy.sort();
    busy
}

/// How a run's cleanup ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CleanupOutcome {
    /// The cleanup's exit status; `None` when it timed out, was killed, or never started.
    pub(crate) exit_code: Option<i32>,
    /// Whether it ran past its timeout and was stopped.
    pub(crate) timed_out: bool,
    /// Why it could not be started, or had to be killed by the supervisor.
    pub(crate) error: Option<String>,
}

impl CleanupOutcome {
    fn failed(error: String) -> Self {
        Self {
            exit_code: None,
            timed_out: false,
            error: Some(error),
        }
    }

    /// Whether the cleanup exited with status 0 within its timeout.
    pub(crate) fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out && self.error.is_none()
    }

    /// One line describing how the cleanup ended, given the timeout it ran under.
    pub(crate) fn describe(&self, timeout: Option<Duration>) -> String {
        if let Some(error) = &self.error {
            return format!("cleanup failed: {error}");
        }
        if self.timed_out {
            return timeout.map_or_else(
                || "cleanup timed out".to_string(),
                |timeout| {
                    format!(
                        "cleanup timed out after {}",
                        humantime::format_duration(timeout)
                    )
                },
            );
        }
        match self.exit_code {
            Some(0) => "cleanup finished".to_string(),
            Some(code) => format!("cleanup failed with exit code {code}"),
            None => "cleanup was killed by a signal".to_string(),
        }
    }
}

/// Everything the termination task needs to run a run's cleanup, captured when the run starts.
pub(crate) struct CleanupTask {
    pub(crate) spec: CleanupSpec,
    pub(crate) run_generation: u64,
    pub(crate) stop_signal: StopSignal,
    /// The environment the service's process was started with.
    pub(crate) environment: HashMap<String, String>,
    pub(crate) working_dir: SpawnWorkingDirectory,
    /// The run's log, which receives the cleanup's output as `[cleanup]` lines.
    pub(crate) sink: RunSink,
}

/// A cleanup whose log is started and whose shim, under containment, is waiting for its go.
pub(crate) struct PreparedCleanup {
    task: CleanupTask,
    tail: Option<LogTail>,
    launch: Launch,
}

enum Launch {
    /// A cleanup-only shim that runs the command once it reads [`GO`] or its stdin closes.
    Shim(Box<ShimProcess>),
    /// Containment is off: the supervisor runs the command itself.
    Direct,
    /// The cleanup-only shim could not be started.
    Failed(String),
}

struct ShimProcess {
    child: tokio::process::Child,
    go: Option<tokio::process::ChildStdin>,
}

impl CleanupTask {
    /// Starts the cleanup's log and, under containment, its cleanup-only shim.
    ///
    /// The shim takes the cleanup lock and waits for [`PreparedCleanup::run`] to say go, or for
    /// the supervisor to die, so a supervisor killed from here on still leaves the cleanup to it.
    pub(crate) fn prepare(self) -> PreparedCleanup {
        let tail = self.spec.files.as_ref().and_then(|files| {
            LogTail::start(
                &files.log,
                &header(Some(self.run_generation), &self.spec.command),
            )
            .inspect_err(|err| {
                tracing::warn!(?err, path = %files.log.display(), "cannot start the cleanup log");
            })
            .ok()
        });
        let launch = match crate::containment::wrap_cleanup(&self.spec, self.stop_signal) {
            Some(wrapped) => self.spawn_shim(wrapped).unwrap_or_else(|err| {
                let error = format!("cannot start the cleanup shim: {err}");
                if let Some(files) = &self.spec.files {
                    append_line(&files.log, &footer(&Ending::CannotRun(err.to_string())));
                }
                Launch::Failed(error)
            }),
            None => Launch::Direct,
        };
        PreparedCleanup {
            task: self,
            tail,
            launch,
        }
    }

    fn spawn_shim(&self, wrapped: crate::containment::Wrapped) -> std::io::Result<Launch> {
        let mut command = tokio::process::Command::new(&wrapped.program);
        command
            .args(&wrapped.args)
            .envs(&self.environment)
            .envs(wrapped.env)
            .current_dir(self.working_dir.as_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Out of the terminal's foreground group, so a Ctrl-C meant for the supervisor does not
        // interrupt the cleanup; and not killed on drop, so it outlives a supervisor that exits.
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn()?;
        let go = child.stdin.take();
        Ok(Launch::Shim(Box::new(ShimProcess { child, go })))
    }
}

impl PreparedCleanup {
    /// Runs the cleanup to its end, mirroring its output into the run's log.
    ///
    /// Neither the run's termination nor a session shutdown cuts it short: its own timeout bounds
    /// it, and the supervisor kills a cleanup-only shim that overstays that by
    /// [`CLEANUP_STOP_GRACE`] plus some slack.
    pub(crate) async fn run(self) -> CleanupOutcome {
        let Self {
            task,
            mut tail,
            launch,
        } = self;
        let outcome = match launch {
            Launch::Shim(shim) => run_shim(&task, &mut tail, *shim).await,
            Launch::Direct => run_direct(&task, &mut tail).await,
            Launch::Failed(error) => CleanupOutcome::failed(error),
        };
        if let Some(tail) = tail {
            tail.finish(&task.sink);
        }
        outcome
    }
}

async fn run_shim(
    task: &CleanupTask,
    tail: &mut Option<LogTail>,
    ShimProcess { mut child, go }: ShimProcess,
) -> CleanupOutcome {
    if let Some(mut go) = go {
        // A shim that already exited has nothing to start; its exit status says why.
        let _ = go.write_all(&[GO]).await;
    }
    let started = tokio::time::Instant::now();
    let bound = task
        .spec
        .timeout
        .saturating_add(CLEANUP_STOP_GRACE)
        .saturating_add(SHIM_SLACK);
    let waited = tail_while(&task.sink, tail, tokio::time::timeout(bound, child.wait())).await;
    let (status, error) = match waited {
        Ok(Ok(status)) => (Some(status), None),
        Ok(Err(err)) => (None, Some(format!("cannot wait for the cleanup: {err}"))),
        Err(_) => {
            // Last resort for a shim that stopped enforcing its own timeout.
            kill_group(&mut child);
            let status = tokio::time::timeout(KILL_WAIT, child.wait())
                .await
                .ok()
                .and_then(Result::ok);
            (
                status,
                Some(format!(
                    "the cleanup ran past {}; killed it",
                    humantime::format_duration(bound)
                )),
            )
        }
    };
    let code = status.and_then(|status| status.code());
    // The shim reports a timeout as 124, which a fast cleanup could also exit with.
    let timed_out = code == Some(EXIT_TIMED_OUT) && started.elapsed() >= task.spec.timeout;
    CleanupOutcome {
        exit_code: code.filter(|_| !timed_out),
        timed_out,
        error,
    }
}

/// Kills a cleanup-only shim together with the cleanup in its process group.
fn kill_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let _ = child.start_kill();
}

async fn run_direct(task: &CleanupTask, tail: &mut Option<LogTail>) -> CleanupOutcome {
    let log = task.spec.files.as_ref().map(|files| files.log.as_path());
    let cannot_run = |error: String| {
        if let Some(log) = log {
            append_line(log, &footer(&Ending::CannotRun(error.clone())));
        }
        CleanupOutcome::failed(format!("cannot run the cleanup: {error}"))
    };
    let Some((program, args)) = task.spec.command.split_first() else {
        return cannot_run("the command is empty".to_string());
    };
    let output = || -> std::io::Result<Stdio> {
        log.map_or_else(
            || Ok(Stdio::null()),
            |log| open_log(log, false).map(Stdio::from),
        )
    };
    let (stdout, stderr) = match output().and_then(|stdout| Ok((stdout, output()?))) {
        Ok(output) => output,
        Err(err) => return cannot_run(err.to_string()),
    };
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .envs(&task.environment)
        .current_dir(task.working_dir.as_path())
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return cannot_run(err.to_string()),
    };
    #[cfg(windows)]
    let job = {
        let attached = child
            .raw_handle()
            .ok_or_else(|| "the cleanup exposed no process handle".to_string())
            .and_then(|handle| {
                crate::windows_job::attach_kill_on_close(handle as isize)
                    .map_err(|err| err.to_string())
            });
        match attached {
            Ok(job) => job,
            Err(error) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return cannot_run(error);
            }
        }
    };

    let (status, timed_out) = tail_while(
        &task.sink,
        tail,
        wait_direct(
            &mut child,
            task.spec.timeout,
            task.stop_signal,
            #[cfg(windows)]
            job,
        ),
    )
    .await;

    let ending = match &status {
        _ if timed_out => Ending::TimedOut(task.spec.timeout),
        Ok(status) => ending_of(*status),
        Err(err) => Ending::CannotRun(format!("cannot wait for it: {err}")),
    };
    if let Some(log) = log {
        append_line(log, &footer(&ending));
    }
    match status {
        Ok(status) => CleanupOutcome {
            exit_code: status.code().filter(|_| !timed_out),
            timed_out,
            error: None,
        },
        Err(err) => CleanupOutcome {
            exit_code: None,
            timed_out,
            error: Some(format!("cannot wait for the cleanup: {err}")),
        },
    }
}

fn ending_of(status: std::process::ExitStatus) -> Ending {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt as _;

        if let Some(signal) = status.signal() {
            return Ending::Signaled(nix::sys::signal::Signal::try_from(signal).map_or_else(
                |_| format!("signal {signal}"),
                |signal| signal.as_str().to_string(),
            ));
        }
    }
    Ending::Exited(status.code().unwrap_or(-1))
}

/// Waits for a directly run cleanup, stopping it once it runs past `timeout`, and returns its
/// status and whether it timed out.
///
/// Whatever the cleanup leaves in its process group is killed once it exits.
#[cfg(unix)]
async fn wait_direct(
    child: &mut tokio::process::Child,
    timeout: Duration,
    stop_signal: StopSignal,
) -> (std::io::Result<std::process::ExitStatus>, bool) {
    use crate::health_check::wait_for_probe_status;
    use nix::sys::signal::Signal;

    let signal_group = |child: &tokio::process::Child, signal: Signal| {
        // The leader is not reaped yet, so its pid still names its group.
        if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
            let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), signal);
        }
    };
    if let Ok(status) = tokio::time::timeout(timeout, wait_for_probe_status(child)).await {
        return (status, false);
    }
    signal_group(child, stop_signal.to_nix());
    if let Ok(status) = tokio::time::timeout(CLEANUP_STOP_GRACE, wait_for_probe_status(child)).await
    {
        return (status, true);
    }
    signal_group(child, Signal::SIGKILL);
    (wait_for_probe_status(child).await, true)
}

/// Waits for a directly run cleanup, terminating its job once it runs past `timeout`, and
/// returns its status and whether it timed out.
///
/// Closing the job also kills whatever the cleanup leaves behind once it exits.
#[cfg(windows)]
async fn wait_direct(
    child: &mut tokio::process::Child,
    timeout: Duration,
    _stop_signal: StopSignal,
    job: win32job::Job,
) -> (std::io::Result<std::process::ExitStatus>, bool) {
    if let Ok(status) = tokio::time::timeout(timeout, child.wait()).await {
        drop(job);
        return (status, false);
    }
    drop(job);
    let _ = child.start_kill();
    (child.wait().await, true)
}

/// Waits for `wait`, mirroring new cleanup output into the run's log meanwhile.
async fn tail_while<T>(
    sink: &RunSink,
    tail: &mut Option<LogTail>,
    wait: impl Future<Output = T>,
) -> T {
    let mut wait = std::pin::pin!(wait);
    let mut ticks = tokio::time::interval(TAIL_INTERVAL);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            output = &mut wait => return output,
            _ = ticks.tick() => {
                if let Some(tail) = tail {
                    tail.poll(sink);
                }
            }
        }
    }
}

/// Mirrors a cleanup log into the run's log, line by line.
struct LogTail {
    /// Read handle positioned after what was already mirrored.
    file: File,
    partial: Vec<u8>,
}

impl LogTail {
    /// Truncates the log at `path` and writes `header`, which stays out of the run's log.
    fn start(path: &Path, header: &str) -> std::io::Result<Self> {
        let mut log = open_log(path, true)?;
        writeln!(log, "{header}")?;
        let mut file = File::open(path)?;
        let written = log.metadata()?.len();
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(written))?;
        Ok(Self {
            file,
            partial: Vec::new(),
        })
    }

    fn poll(&mut self, sink: &RunSink) {
        let mut appended = Vec::new();
        if let Err(err) = self.file.read_to_end(&mut appended) {
            tracing::debug!(?err, "cannot read the cleanup log");
            return;
        }
        self.partial.extend_from_slice(&appended);
        while let Some(newline) = self.partial.iter().position(|byte| *byte == b'\n') {
            let rest = self.partial.split_off(newline.saturating_add(1));
            let mut line = std::mem::replace(&mut self.partial, rest);
            line.pop();
            emit(sink, &line);
        }
        if self.partial.len() > LINE_MAX_BYTES {
            emit(sink, &std::mem::take(&mut self.partial));
        }
    }

    fn finish(mut self, sink: &RunSink) {
        self.poll(sink);
        if !self.partial.is_empty() {
            emit(sink, &self.partial);
        }
    }
}

fn emit(sink: &RunSink, line: &[u8]) {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let line = String::from_utf8_lossy(line);
    let line = if line.is_empty() {
        "[cleanup]".to_string()
    } else {
        format!("[cleanup] {line}")
    };
    sink.append_log(OutputStream::Stdout, LogUpdateKind::Append, line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use similar_asserts::assert_eq;

    /// A starting session waits for every cleanup lock a previous session still holds, names the
    /// services it waits for once, and goes on as soon as the lock is released.
    #[tokio::test]
    async fn restarted_session_waits_for_a_held_cleanup_lock() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        // A lock nobody holds, as a finished cleanup leaves it, never delays a start.
        let finished = files_in(directory.path(), "done");
        drop(open_lock(&finished.lock)?);
        let held = files_in(directory.path(), "infra");
        let holder = open_lock(&held.lock)?;
        holder.lock()?;

        let cancel = CancellationToken::new();
        let (waited_tx, mut waited_rx) = tokio::sync::mpsc::unbounded_channel();
        let wait = tokio::spawn({
            let directory = directory.path().to_path_buf();
            let cancel = cancel.clone();
            async move {
                wait_for_locks(&directory, &cancel, |busy| {
                    let _ = waited_tx.send(busy.to_vec());
                })
                .await
            }
        });

        // The wait reports the busy service and keeps waiting while the lock is held.
        let busy = tokio::time::timeout(Duration::from_secs(5), waited_rx.recv())
            .await?
            .ok_or_else(|| eyre::eyre!("the wait never reported the busy lock"))?;
        assert_eq!(busy, vec!["infra".to_string()]);
        tokio::time::sleep(LOCK_POLL_INTERVAL * 2).await;
        assert!(
            !wait.is_finished(),
            "the wait ended while the lock was held"
        );

        // Releasing the lock ends the wait without a second report.
        drop(holder);
        tokio::time::timeout(Duration::from_secs(5), wait).await???;
        assert!(waited_rx.recv().await.is_none());
        Ok(())
    }

    /// Cancelling the token ends a wait for a lock that is never released.
    #[tokio::test]
    async fn waiting_for_a_previous_cleanup_can_be_cancelled() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let held = files_in(directory.path(), "infra");
        let holder = open_lock(&held.lock)?;
        holder.lock()?;

        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_locks(directory.path(), &cancel, |_| {}),
        )
        .await?;
        assert_eq!(result, Err(CleanupWaitCancelled));
        drop(holder);
        Ok(())
    }

    /// An outcome is a success only when the cleanup exited 0 in time, and its description names
    /// what went wrong otherwise.
    #[test]
    fn outcomes_describe_how_the_cleanup_ended() {
        let exited = |code| CleanupOutcome {
            exit_code: Some(code),
            timed_out: false,
            error: None,
        };
        assert!(exited(0).succeeded());
        assert_eq!(exited(0).describe(None), "cleanup finished");
        assert!(!exited(5).succeeded());
        assert_eq!(exited(5).describe(None), "cleanup failed with exit code 5");

        let timed_out = CleanupOutcome {
            exit_code: None,
            timed_out: true,
            error: None,
        };
        assert!(!timed_out.succeeded());
        assert_eq!(
            timed_out.describe(Some(Duration::from_secs(120))),
            "cleanup timed out after 2m"
        );

        let failed = CleanupOutcome::failed("cannot run the cleanup: not found".to_string());
        assert!(!failed.succeeded());
        assert_eq!(
            failed.describe(None),
            "cleanup failed: cannot run the cleanup: not found"
        );
    }
}
