use std::path::{Path, PathBuf};
use std::time::Duration;

use micromux_control::{
    ControlEndpoint, EndpointProbeResult, SessionInfo, endpoint_owner_lock_held, probe_endpoints,
    unique_answering_session_probes,
};

use crate::StartSessionResult;
use crate::select::ToolError;

/// How long `stop_session` waits to confirm the session process actually exited.
#[cfg(unix)]
pub(crate) const STOP_CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);

/// Waits for one observed process identity, using a pidfd where the platform provides one.
///
/// The fallback polls by PID and therefore retains the usual PID-reuse race on non-Linux Unix.
#[cfg(unix)]
pub(crate) struct ProcessExitWatch {
    pid: u32,
    #[cfg(target_os = "linux")]
    pidfd: Option<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>,
    #[cfg(target_os = "linux")]
    already_exited: bool,
}

#[cfg(unix)]
impl ProcessExitWatch {
    pub(crate) fn new(pid: u32) -> Self {
        #[cfg(target_os = "linux")]
        let opened = i32::try_from(pid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .map(|pid| rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()));
        #[cfg(target_os = "linux")]
        let already_exited =
            matches!(&opened, Some(Err(error)) if *error == rustix::io::Errno::SRCH);
        #[cfg(target_os = "linux")]
        let pidfd = opened
            .and_then(Result::ok)
            .and_then(|fd| tokio::io::unix::AsyncFd::new(fd).ok());
        Self {
            pid,
            #[cfg(target_os = "linux")]
            pidfd,
            #[cfg(target_os = "linux")]
            already_exited,
        }
    }

    pub(crate) async fn wait(self, timeout: Duration) -> bool {
        #[cfg(target_os = "linux")]
        if self.already_exited {
            return true;
        }
        #[cfg(target_os = "linux")]
        if let Some(pidfd) = self.pidfd {
            return tokio::time::timeout(timeout, pidfd.readable())
                .await
                .is_ok_and(|ready| ready.is_ok());
        }
        wait_until_stopped(self.pid, timeout).await
    }
}

/// Whether a process is still alive, via a signal-0 `kill`. Used to confirm a stopped session exited
/// (so its ports are freed) before reporting success.
#[cfg(unix)]
pub(crate) fn process_alive(pid: u32) -> bool {
    // Reject pid 0: kill(0, …) targets the caller's own process group, never a session process.
    if pid == 0 {
        return false;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // Signal 0 delivers nothing; it only checks existence/permission. ESRCH means the process is
    // gone; anything else (Ok, or EPERM) means it still exists.
    !matches!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

/// Poll until `pid` is gone or the timeout elapses; returns whether it exited.
#[cfg(unix)]
pub(crate) async fn wait_until_stopped(pid: u32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !process_alive(pid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// How long `start_session` waits for a freshly spawned session's control plane to come up.
pub(crate) const START_READY_TIMEOUT: Duration = Duration::from_secs(15);
/// After a spawned `serve` exits (e.g. it lost the lifetime-lock race to a concurrent start), how
/// long to keep polling for a session to become reachable before reporting failure — long enough for
/// the race winner (which binds within milliseconds) to come up, short enough to fail a bad config
/// quickly.
pub(crate) const CHILD_EXIT_GRACE: Duration = Duration::from_secs(3);

pub(crate) struct SpawnedServe {
    pub(crate) child: tokio::process::Child,
    pub(crate) stderr_path: PathBuf,
}

/// Spawn `micromux serve` detached for a project's config and return its child handle plus the
/// file capturing its stderr. A new process group plus null stdin/stdout detach it from this
/// ephemeral MCP server and keep it off the JSON-RPC stdio channel; stderr goes to a unique temp
/// file so a startup failure can be quoted back to the agent — the caller deletes it on success
/// (`remove_captured_stderr`) and keeps + quotes it on failure (`start_failure_message`).
/// `--config` pins the same endpoint hash the proxy derives, regardless of the child's working
/// directory. Using `tokio::process` means the runtime reaps the child in the background once it
/// exits (no zombie), so a later `kill(pid, 0)` reports it truly gone, and lets `start_session`
/// observe an early exit.
#[cfg(unix)]
pub(crate) fn spawn_detached_serve(config_path: &Path) -> std::io::Result<SpawnedServe> {
    use std::process::Stdio;

    let exe = std::env::current_exe()?;
    let project_dir = config_path.parent().unwrap_or(config_path);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let stderr_path = std::env::temp_dir().join(format!(
        "micromux-serve-{}-{nonce}.stderr",
        std::process::id()
    ));
    let stderr = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&stderr_path)?;
    let child = tokio::process::Command::new(exe)
        .arg("serve")
        .arg("--config")
        .arg(config_path)
        .current_dir(project_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .process_group(0)
        .spawn();
    match child {
        Ok(child) => Ok(SpawnedServe { child, stderr_path }),
        Err(err) => {
            let _ = std::fs::remove_file(&stderr_path);
            Err(err)
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn spawn_detached_serve(_config_path: &Path) -> std::io::Result<SpawnedServe> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "start_session is only supported on unix",
    ))
}

/// Delete the stderr capture once a session is up. The long-lived `serve` keeps the unlinked
/// file open, so anything it writes to stderr later lands in an invisible inode — acceptable
/// because tracing goes to the log file and stderr stays essentially silent after startup.
pub(crate) fn remove_captured_stderr(path: &Path) {
    let _ = std::fs::remove_file(path);
}

pub(crate) fn start_failure_message(base: &str, stderr_path: &Path) -> String {
    const STDERR_TAIL: u64 = 4 * 1024;
    const MAX_MESSAGE: usize = 6 * 1024;

    // Read only the tail: after 15s of readiness polling a spewing child can leave megabytes.
    let stderr = std::fs::File::open(stderr_path)
        .and_then(|mut file| {
            use std::io::{Read, Seek, SeekFrom};
            let len = file.seek(SeekFrom::End(0))?;
            file.seek(SeekFrom::Start(len.saturating_sub(STDERR_TAIL)))?;
            let mut tail = Vec::new();
            file.read_to_end(&mut tail)?;
            Ok(tail)
        })
        .unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr);
    let stderr = strip_ansi_escapes::strip_str(&stderr);
    let log_hint = micromux::project_dir().map_or_else(
        || "session log directory could not be resolved".to_string(),
        |dirs| format!("session logs directory: {}", dirs.cache_dir().display()),
    );
    let mut message = format!(
        "{base}\ncaptured stderr file: {}\n{log_hint}",
        stderr_path.display()
    );
    if !stderr.trim().is_empty() {
        message.push_str("\n--- captured stderr (tail) ---\n");
        message.push_str(stderr.trim());
    }
    if message.len() > MAX_MESSAGE {
        let keep = MAX_MESSAGE.saturating_sub("…\n".len());
        let mut start = message.len().saturating_sub(keep);
        while start < message.len() && !message.is_char_boundary(start) {
            start += 1;
        }
        message = format!("…\n{}", message.get(start..).unwrap_or_default());
    }
    message
}

pub(crate) async fn already_running_any(
    endpoints: &[ControlEndpoint],
    config_path: &Path,
) -> Result<Option<StartSessionResult>, ToolError> {
    let probes = probe_endpoints(endpoints).await;
    let sessions = unique_answering_session_probes(&probes);
    if sessions.len() > 1 {
        return Err(ambiguous_start_session(config_path, sessions.len()));
    }
    if let Some((_endpoint, info)) = sessions.into_iter().next() {
        return Ok(Some(already_running_report(&info)));
    }

    for probe in probes {
        let reason = match probe.result {
            EndpointProbeResult::ProtocolMismatch { peer, ours } => {
                format!("control protocol version mismatch: peer={peer}, ours={ours}")
            }
            EndpointProbeResult::Unreachable(reason) => reason,
            EndpointProbeResult::Session(_) | EndpointProbeResult::Absent(_) => continue,
        };
        if endpoint_owner_lock_held(&probe.endpoint).unwrap_or(false) {
            return Ok(Some(StartSessionResult {
                started: false,
                already_running: Some(true),
                reachable: Some(false),
                config_path: config_path.display().to_string(),
                session: None,
                id: None,
                pid: None,
                endpoint: Some(probe.endpoint.to_string()),
                reason: Some(reason),
            }));
        }
    }

    Ok(None)
}

pub(crate) async fn reachable_session_for_start(
    endpoints: &[ControlEndpoint],
    config_path: &Path,
    child_pid: Option<u32>,
) -> Result<Option<(bool, SessionInfo)>, ToolError> {
    let probes = probe_endpoints(endpoints).await;
    let sessions = unique_answering_session_probes(&probes);
    if sessions.len() > 1 {
        return Err(ambiguous_start_session(config_path, sessions.len()));
    }
    let Some((_endpoint, info)) = sessions.into_iter().next() else {
        return Ok(None);
    };
    Ok(Some((child_pid == Some(info.pid), info)))
}

pub(crate) fn start_session_report(info: &SessionInfo, started: bool) -> StartSessionResult {
    if started {
        StartSessionResult {
            started: true,
            already_running: None,
            reachable: None,
            config_path: info.config_path.clone(),
            session: Some(info.name.clone()),
            id: Some(info.id.clone()),
            pid: Some(info.pid),
            endpoint: None,
            reason: None,
        }
    } else {
        already_running_report(info)
    }
}

fn already_running_report(info: &SessionInfo) -> StartSessionResult {
    StartSessionResult {
        started: false,
        already_running: Some(true),
        reachable: None,
        config_path: info.config_path.clone(),
        session: Some(info.name.clone()),
        id: Some(info.id.clone()),
        pid: Some(info.pid),
        endpoint: None,
        reason: None,
    }
}

fn ambiguous_start_session(config_path: &Path, sessions: usize) -> ToolError {
    ToolError::Ambiguous(format!(
        "config {} matched {sessions} live sessions; use list_sessions and stop the duplicate before starting another",
        config_path.display(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // An end-to-end start_session failure test (spawn a real serve against a broken config and
    // assert the tool error quotes its stderr) is deliberately absent: `spawn_detached_serve`
    // execs `current_exe()`, which under `cargo test` is the test binary, not micromux. The
    // message shaping below is the unit-testable half; the spawn path is covered manually.

    #[test]
    fn startup_failure_includes_bounded_ansi_free_stderr_tail() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("serve.stderr");
        let noisy = format!(
            "{}\u{1b}[31mactual config failure\u{1b}[0m\n",
            "x".repeat(5000)
        );
        std::fs::write(&path, noisy)?;

        let message = start_failure_message("serve exited", &path);
        assert!(message.contains("actual config failure"));
        assert!(message.contains(&path.display().to_string()));
        assert!(message.contains("session logs directory"));
        assert!(!message.contains("\u{1b}[31m"));
        assert!(message.len() <= 6 * 1024);
        Ok(())
    }
}
