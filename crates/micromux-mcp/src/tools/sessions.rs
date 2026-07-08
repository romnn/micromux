use std::path::Path;
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

/// Spawn `micromux serve` detached for a project's config and return its child handle. A new process
/// group + null stdio detach it from this ephemeral MCP server and keep it off the JSON-RPC stdio
/// channel; `--config` pins the same endpoint hash the proxy derives, regardless of the child's
/// working directory. Using `tokio::process` means the runtime reaps the child in the background once
/// it exits (no zombie), so a later `kill(pid, 0)` reports it truly gone, and lets `start_session`
/// observe an early exit.
#[cfg(unix)]
pub(crate) fn spawn_detached_serve(config_path: &Path) -> std::io::Result<tokio::process::Child> {
    use std::process::Stdio;

    let exe = std::env::current_exe()?;
    let project_dir = config_path.parent().unwrap_or(config_path);
    tokio::process::Command::new(exe)
        .arg("serve")
        .arg("--config")
        .arg(config_path)
        .current_dir(project_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
}

#[cfg(not(unix))]
pub(crate) fn spawn_detached_serve(_config_path: &Path) -> std::io::Result<tokio::process::Child> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "start_session is only supported on unix",
    ))
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
