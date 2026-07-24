//! Read-only, connect-to-verify session selection.
//!
//! The control crate owns selector parsing and endpoint resolution. This layer applies the
//! `MICROMUX_SESSION` fallback and enriches transport probe reports with agent-facing diagnostics;
//! it never prunes endpoints or mutates sessions.

use std::path::{Path, PathBuf};

use micromux_control::{
    ControlError, EndpointProbe, EndpointProbeResult, ProbeReport, ResolvedSession,
    RuntimeDirDetail, RuntimeDirStatus, SelectError, SessionInfo, SessionSelector,
    SocketProbeDetail, probe_detail, probe_runtime_dirs, resolve_selector, runtime_dir_statuses,
    unique_answering_session_probes, usable_runtime_dirs,
};
#[cfg(all(test, unix))]
use micromux_control::{
    ProtocolVersion, SocketProbeStatus, resolve_selector_in_runtime_dirs,
    session_config_path_is_under, session_matches_config_path,
};
use schemars::JsonSchema;
use serde::Serialize;

/// A typed tool error surfaced to the agent.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// No live session matched.
    #[error("no session: {}", .0.summary)]
    NoSession(Box<DiscoveryDiagnostics>),
    /// A name matched more than one live session.
    #[error("ambiguous selector: {0}")]
    Ambiguous(String),
    /// A session endpoint exists but did not answer promptly.
    #[error("session busy: {0}")]
    Busy(String),
    /// The control plane is not supported on this platform.
    #[error("the micromux control plane is not supported on this platform")]
    Unsupported,
    /// The command is invalid in the service's current state.
    #[error("invalid state: {0}")]
    InvalidState(String),
    /// The latest micromux config could not be reloaded before a command.
    #[error("config reload failed: {0}")]
    ConfigReload(String),
    /// An error reported by the session.
    #[error("{code:?}: {message}")]
    Remote {
        /// The protocol error code.
        code: micromux_control::ErrorCode,
        /// A human-readable message.
        message: String,
    },
    /// A transport error.
    #[error(transparent)]
    Control(ControlError),
    /// A response that did not match the request.
    #[error("unexpected response: {0}")]
    Unexpected(String),
}

/// Structured diagnostics for session discovery.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DiscoveryDiagnostics {
    /// Human summary of what was being resolved.
    pub summary: String,
    /// Current executable path.
    pub executable: String,
    /// Current binary version.
    pub version: &'static str,
    /// Current working directory.
    pub cwd: String,
    /// Canonical config path, if resolution reached one.
    pub config_path: Option<String>,
    /// The process environment's `XDG_RUNTIME_DIR`.
    pub xdg_runtime_dir: Option<String>,
    /// All candidate runtime directories and whether they were usable.
    pub runtime_dirs: Vec<RuntimeDirDetail>,
    /// Probe result for each derived or discovered socket.
    pub socket_probes: Vec<SocketProbeDetail>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SessionList {
    pub sessions: Vec<SessionInfo>,
    pub diagnostics: DiscoveryDiagnostics,
}

impl From<ControlError> for ToolError {
    fn from(err: ControlError) -> Self {
        match err {
            ControlError::Unsupported => Self::Unsupported,
            ControlError::Timeout => {
                Self::Busy("the micromux session did not answer in time".to_string())
            }
            other => Self::Control(other),
        }
    }
}

pub(crate) type Resolved = ResolvedSession;

pub(crate) use micromux_control::config_for_target;

fn selector_with_environment_fallback(raw: Option<String>) -> Option<SessionSelector> {
    raw.or_else(|| std::env::var("MICROMUX_SESSION").ok())
        .as_deref()
        .map(SessionSelector::parse)
}

fn diagnostics_from_report(cwd: &Path, report: ProbeReport) -> DiscoveryDiagnostics {
    DiscoveryDiagnostics {
        summary: report.summary,
        executable: std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string()),
        version: env!("CARGO_PKG_VERSION"),
        cwd: cwd.display().to_string(),
        config_path: report.config_path,
        xdg_runtime_dir: std::env::var("XDG_RUNTIME_DIR").ok(),
        runtime_dirs: report.runtime_dirs,
        socket_probes: report.socket_probes,
    }
}

fn map_select_error(cwd: &Path, error: SelectError) -> ToolError {
    match error {
        SelectError::NoSession(report) => {
            ToolError::NoSession(Box::new(diagnostics_from_report(cwd, *report)))
        }
        SelectError::Ambiguous(ambiguous) => ToolError::Ambiguous(ambiguous.message),
        SelectError::Busy(message) => ToolError::Busy(message),
        SelectError::Unsupported => ToolError::Unsupported,
        SelectError::Control(error) => ToolError::from(error),
    }
}

/// Resolve a selector to a single live session.
///
/// # Errors
///
/// Returns [`ToolError::NoSession`] when nothing answers, [`ToolError::Ambiguous`] when a name
/// matches more than one, or [`ToolError::Unsupported`] on a platform without a transport.
pub async fn resolve(cwd: &Path, selector: Option<String>) -> Result<Resolved, ToolError> {
    let selector = selector_with_environment_fallback(selector);
    resolve_selector(cwd, selector, None)
        .await
        .map_err(|error| map_select_error(cwd, error))
}

/// Resolve a selector against an explicit runtime directory (testable variant of [`resolve`]).
///
/// # Errors
///
/// See [`resolve`].
#[cfg(all(test, unix))]
pub(crate) async fn resolve_in(
    runtime_dir: &Path,
    cwd: &Path,
    selector: Option<String>,
) -> Result<Resolved, ToolError> {
    let runtime_dirs = vec![runtime_dir.to_path_buf()];
    let dir_statuses = vec![RuntimeDirStatus {
        path: runtime_dir.to_path_buf(),
        usable: true,
        error: None,
    }];
    resolve_in_dirs(&runtime_dirs, &dir_statuses, cwd, selector).await
}

#[cfg(all(test, unix))]
async fn resolve_in_dirs(
    runtime_dirs: &[PathBuf],
    dir_statuses: &[RuntimeDirStatus],
    cwd: &Path,
    selector: Option<String>,
) -> Result<Resolved, ToolError> {
    let selector = selector_with_environment_fallback(selector);
    resolve_selector_in_runtime_dirs(runtime_dirs, dir_statuses, cwd, selector, None)
        .await
        .map_err(|error| map_select_error(cwd, error))
}

pub(crate) fn discovery_diagnostics(
    cwd: &Path,
    summary: String,
    config_path: Option<&Path>,
    dir_statuses: &[RuntimeDirStatus],
    socket_probes: Vec<SocketProbeDetail>,
) -> DiscoveryDiagnostics {
    DiscoveryDiagnostics {
        summary,
        executable: std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string()),
        version: env!("CARGO_PKG_VERSION"),
        cwd: cwd.display().to_string(),
        config_path: config_path.map(|path| path.display().to_string()),
        xdg_runtime_dir: std::env::var("XDG_RUNTIME_DIR").ok(),
        runtime_dirs: dir_statuses
            .iter()
            .map(|status| RuntimeDirDetail {
                path: status.path.display().to_string(),
                usable: status.usable,
                error: status.error.clone(),
            })
            .collect(),
        socket_probes,
    }
}
fn session_infos_from_probes(probes: &[EndpointProbe]) -> Vec<SessionInfo> {
    unique_answering_session_probes(probes)
        .into_iter()
        .map(|(_endpoint, info)| info)
        .collect()
}

/// List every session that answers `Describe`. Endpoints that refuse a connection are skipped;
/// live-but-unusable ones are simply not listed this scan (never pruned).
///
/// # Errors
///
/// Returns [`ToolError::Unsupported`] on a platform without a control transport.
pub async fn list_sessions() -> Result<SessionList, ToolError> {
    if !micromux_control::transport_supported() {
        return Err(ToolError::Unsupported);
    }
    let dir_statuses = runtime_dir_statuses();
    let runtime_dirs = usable_runtime_dirs(&dir_statuses);
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if runtime_dirs.is_empty() {
        return Err(ToolError::NoSession(Box::new(discovery_diagnostics(
            &cwd,
            "no runtime directory could be resolved".to_string(),
            None,
            &dir_statuses,
            Vec::new(),
        ))));
    }
    let probes = probe_runtime_dirs(&runtime_dirs).await?;
    let sessions = session_infos_from_probes(&probes);
    let summary = session_list_summary(&sessions, &probes);
    let socket_probes = probes.into_iter().map(probe_detail).collect();
    Ok(SessionList {
        sessions,
        diagnostics: discovery_diagnostics(&cwd, summary, None, &dir_statuses, socket_probes),
    })
}

/// List every answering session together with the endpoint that drives it. Like [`list_sessions`]
/// but keeps the endpoint so a caller can query each session — used to locate a service across
/// sibling sessions. An empty vector means no runtime dir or no answering session, never an error.
///
/// # Errors
///
/// Returns [`ToolError::Unsupported`] on a platform without a control transport.
pub async fn answering_sessions() -> Result<Vec<Resolved>, ToolError> {
    if !micromux_control::transport_supported() {
        return Err(ToolError::Unsupported);
    }
    let dir_statuses = runtime_dir_statuses();
    let runtime_dirs = usable_runtime_dirs(&dir_statuses);
    if runtime_dirs.is_empty() {
        return Ok(Vec::new());
    }
    let probes = probe_runtime_dirs(&runtime_dirs).await?;
    Ok(unique_answering_session_probes(&probes)
        .into_iter()
        .map(|(endpoint, info)| ResolvedSession { endpoint, info })
        .collect())
}

fn session_list_summary(sessions: &[SessionInfo], probes: &[EndpointProbe]) -> String {
    if !sessions.is_empty() {
        return format!("found {} answering micromux session(s)", sessions.len());
    }
    let unusable = probes
        .iter()
        .filter(|probe| {
            matches!(
                probe.result,
                EndpointProbeResult::ProtocolMismatch { .. } | EndpointProbeResult::Unreachable(_)
            )
        })
        .count();
    let absent = probes
        .iter()
        .filter(|probe| matches!(probe.result, EndpointProbeResult::Absent(_)))
        .count();
    if unusable > 0 {
        format!(
            "found {unusable} socket(s) that exist but could not be used; inspect socket_probes for protocol/version/busy details"
        )
    } else if absent > 0 {
        format!("found {absent} stale socket(s), but no answering micromux session")
    } else {
        "no micromux session sockets found".to_string()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Arc;

    use color_eyre::eyre;
    use fs2::FileExt;
    use micromux::CancellationToken;
    use micromux_control::{
        ControlEndpoint, ControlServer, SessionIdentity, bind, endpoint_for, endpoint_hash,
    };
    use similar_asserts::assert_eq;
    use tokio::io::AsyncWriteExt;

    struct Running {
        shutdown: CancellationToken,
        _runner: tokio::task::JoinHandle<Result<(), micromux::Error>>,
    }

    fn temp_dir(prefix: &str) -> eyre::Result<tempfile::TempDir> {
        Ok(tempfile::Builder::new()
            .prefix(&format!("micromux-mcp-{prefix}-"))
            .tempdir()?)
    }

    fn boot(runtime_dir: &Path, name: &str, config_path: &Path) -> eyre::Result<Running> {
        boot_at_endpoint(&endpoint_for(runtime_dir, config_path), name, config_path)
    }

    fn boot_at_endpoint(
        endpoint: &ControlEndpoint,
        name: &str,
        config_path: &Path,
    ) -> eyre::Result<Running> {
        boot_at_endpoint_with_working_dir(endpoint, name, config_path, Path::new("."))
    }

    fn boot_at_endpoint_with_working_dir(
        endpoint: &ControlEndpoint,
        name: &str,
        config_path: &Path,
        working_dir: &Path,
    ) -> eyre::Result<Running> {
        let yaml = "version: 1\nservices:\n  svc:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n";
        let mut diagnostics = vec![];
        let config = micromux::from_str(yaml, working_dir, 0usize, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!("parse: {err}"))?;
        let mux = Arc::new(micromux::Micromux::new(&config)?);
        let shutdown = CancellationToken::new();
        let (runner, handles) = mux.clone().start(shutdown.clone());
        let runner = tokio::spawn(runner);

        let guard = bind(endpoint)?.ok_or_else(|| eyre::eyre!("bind failed"))?;
        let identity = SessionIdentity::new(name.to_string(), working_dir, config_path);
        let server = Arc::new(ControlServer::new(
            handles.reader.clone(),
            handles.service_control(),
            identity,
            handles.dynamic_services.clone(),
        ));
        tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                let _ = server.serve(guard, shutdown).await;
            }
        });
        Ok(Running {
            shutdown,
            _runner: runner,
        })
    }

    #[tokio::test]
    async fn resolves_by_name_skips_dead_and_detects_ambiguity() -> eyre::Result<()> {
        let runtime_dir = temp_dir("select")?;
        let config_a = runtime_dir.path().join("proj-a/micromux.yaml");
        let alpha = boot(runtime_dir.path(), "alpha", &config_a)?;

        // A leaked, non-connectable socket file must be skipped, never resolved or pruned.
        std::fs::write(runtime_dir.path().join("dead.sock"), b"")?;

        let resolved = resolve_in(
            runtime_dir.path(),
            Path::new("."),
            Some("alpha".to_string()),
        )
        .await?;
        assert_eq!(resolved.info.name, "alpha");

        let missing =
            resolve_in(runtime_dir.path(), Path::new("."), Some("nope".to_string())).await;
        assert!(matches!(missing, Err(ToolError::NoSession(_))));

        // A second session sharing the name must be reported Ambiguous, never picked arbitrarily.
        let config_b = runtime_dir.path().join("proj-b/micromux.yaml");
        let beta = boot(runtime_dir.path(), "alpha", &config_b)?;
        let ambiguous = resolve_in(
            runtime_dir.path(),
            Path::new("."),
            Some("alpha".to_string()),
        )
        .await;
        assert!(matches!(ambiguous, Err(ToolError::Ambiguous(_))));

        alpha.shutdown.cancel();
        beta.shutdown.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn current_selector_checks_later_runtime_dirs_after_unreachable_endpoint()
    -> eyre::Result<()> {
        let first_runtime = temp_dir("current-first")?;
        let second_runtime = temp_dir("current-second")?;
        let project_dir = temp_dir("current-project")?;
        std::fs::write(
            project_dir.path().join("micromux.yaml"),
            "version: 1\nservices:\n  svc:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
        )?;
        let config_path = std::fs::canonicalize(project_dir.path().join("micromux.yaml"))?;
        let session = boot(second_runtime.path(), "live", &config_path)?;

        let ControlEndpoint::Unix(garbage_path) = endpoint_for(first_runtime.path(), &config_path)
        else {
            eyre::bail!("expected unix endpoint");
        };
        let listener = tokio::net::UnixListener::bind(&garbage_path)?;
        let garbage = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream.write_all(b"not json\n").await;
                let _ = stream.flush().await;
            }
        });

        let runtime_dirs = vec![
            first_runtime.path().to_path_buf(),
            second_runtime.path().to_path_buf(),
        ];
        let dir_statuses = runtime_dirs
            .iter()
            .cloned()
            .map(|path| RuntimeDirStatus {
                path,
                usable: true,
                error: None,
            })
            .collect::<Vec<_>>();
        let resolved =
            resolve_in_dirs(&runtime_dirs, &dir_statuses, project_dir.path(), None).await?;
        assert_eq!(resolved.info.name, "live");

        garbage.abort();
        session.shutdown.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn current_selector_scans_for_unique_session_when_direct_endpoint_is_unusable()
    -> eyre::Result<()> {
        let runtime_dir = temp_dir("current-scan-fallback")?;
        let project_dir = temp_dir("current-scan-fallback-project")?;
        std::fs::write(
            project_dir.path().join("micromux.yaml"),
            "version: 1\nservices:\n  svc:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
        )?;
        let config_path = std::fs::canonicalize(project_dir.path().join("micromux.yaml"))?;

        let ControlEndpoint::Unix(direct_path) = endpoint_for(runtime_dir.path(), &config_path)
        else {
            eyre::bail!("expected unix endpoint");
        };
        let listener = tokio::net::UnixListener::bind(&direct_path)?;
        let garbage = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream.write_all(b"not json\n").await;
                let _ = stream.flush().await;
            }
        });

        let alias_endpoint = ControlEndpoint::Unix(runtime_dir.path().join("session-alias.sock"));
        let session = boot_at_endpoint(&alias_endpoint, "live", &config_path)?;
        let runtime_dirs = vec![runtime_dir.path().to_path_buf()];
        let dir_statuses = runtime_dirs
            .iter()
            .cloned()
            .map(|path| RuntimeDirStatus {
                path,
                usable: true,
                error: None,
            })
            .collect::<Vec<_>>();

        let resolved =
            resolve_in_dirs(&runtime_dirs, &dir_statuses, project_dir.path(), None).await?;
        assert_eq!(resolved.info.name, "live");
        assert_eq!(Path::new(&resolved.info.config_path), config_path);

        garbage.abort();
        session.shutdown.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn current_selector_scans_for_unique_subconfig_when_current_config_is_absent()
    -> eyre::Result<()> {
        let runtime_dir = temp_dir("current-subconfig-fallback")?;
        let project_dir = temp_dir("current-subconfig-project")?;
        std::fs::create_dir(project_dir.path().join("tools"))?;
        std::fs::write(
            project_dir.path().join("micromux.yaml"),
            "version: 1\nservices:\n  stale:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
        )?;
        std::fs::write(
            project_dir.path().join("tools/micromux.yaml"),
            "version: 1\nservices:\n  live:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
        )?;
        let live_config = std::fs::canonicalize(project_dir.path().join("tools/micromux.yaml"))?;

        let alias_endpoint = ControlEndpoint::Unix(runtime_dir.path().join("tools-session.sock"));
        let session = boot_at_endpoint_with_working_dir(
            &alias_endpoint,
            "live",
            &live_config,
            project_dir.path(),
        )?;
        let runtime_dirs = vec![runtime_dir.path().to_path_buf()];
        let dir_statuses = runtime_dirs
            .iter()
            .cloned()
            .map(|path| RuntimeDirStatus {
                path,
                usable: true,
                error: None,
            })
            .collect::<Vec<_>>();

        let resolved =
            resolve_in_dirs(&runtime_dirs, &dir_statuses, project_dir.path(), None).await?;
        assert_eq!(resolved.info.name, "live");
        assert_eq!(Path::new(&resolved.info.config_path), live_config);

        session.shutdown.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn current_selector_rejects_malformed_current_config_protocol_version() -> eyre::Result<()>
    {
        let runtime_dir = temp_dir("current-malformed-protocol-no-subconfig-fallback")?;
        let project_dir = temp_dir("current-malformed-protocol-project")?;
        std::fs::create_dir(project_dir.path().join("tools"))?;
        std::fs::write(
            project_dir.path().join("micromux.yaml"),
            "version: 1\nservices:\n  stale:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
        )?;
        std::fs::write(
            project_dir.path().join("tools/micromux.yaml"),
            "version: 1\nservices:\n  live:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
        )?;
        let current_config = std::fs::canonicalize(project_dir.path().join("micromux.yaml"))?;
        let live_config = std::fs::canonicalize(project_dir.path().join("tools/micromux.yaml"))?;

        let ControlEndpoint::Unix(direct_path) = endpoint_for(runtime_dir.path(), &current_config)
        else {
            eyre::bail!("expected unix endpoint");
        };
        let listener = tokio::net::UnixListener::bind(&direct_path)?;
        let malformed_response = serde_json::to_vec(&serde_json::json!({
            "Description": {
                "protocol_version": 4,
                "id": endpoint_hash(&current_config),
                "pid": 42,
                "start_time": 99,
                "name": "malformed-root",
                "working_dir": project_dir.path(),
                "config_path": current_config,
                "services": [],
                "micromux_version": "malformed"
            }
        }))?;
        let malformed_socket = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream.write_all(&malformed_response).await;
                let _ = stream.write_all(b"\n").await;
                let _ = stream.flush().await;
            }
        });

        let alias_endpoint = ControlEndpoint::Unix(runtime_dir.path().join("tools-session.sock"));
        let session = boot_at_endpoint_with_working_dir(
            &alias_endpoint,
            "live",
            &live_config,
            project_dir.path(),
        )?;
        let runtime_dirs = vec![runtime_dir.path().to_path_buf()];
        let dir_statuses = runtime_dirs
            .iter()
            .cloned()
            .map(|path| RuntimeDirStatus {
                path,
                usable: true,
                error: None,
            })
            .collect::<Vec<_>>();

        let resolved =
            resolve_in_dirs(&runtime_dirs, &dir_statuses, project_dir.path(), None).await;
        let Err(ToolError::Busy(message)) = resolved else {
            eyre::bail!("expected malformed current config socket to block fallback");
        };
        assert!(message.contains("the current config"));
        assert!(message.contains("invalid type"));

        malformed_socket.abort();
        session.shutdown.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn current_selector_does_not_retarget_on_current_config_protocol_mismatch()
    -> eyre::Result<()> {
        let runtime_dir = temp_dir("current-protocol-mismatch-no-subconfig-fallback")?;
        let project_dir = temp_dir("current-protocol-mismatch-project")?;
        std::fs::create_dir(project_dir.path().join("tools"))?;
        std::fs::write(
            project_dir.path().join("micromux.yaml"),
            "version: 1\nservices:\n  stale:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
        )?;
        std::fs::write(
            project_dir.path().join("tools/micromux.yaml"),
            "version: 1\nservices:\n  live:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
        )?;
        let current_config = std::fs::canonicalize(project_dir.path().join("micromux.yaml"))?;
        let live_config = std::fs::canonicalize(project_dir.path().join("tools/micromux.yaml"))?;

        let ControlEndpoint::Unix(direct_path) = endpoint_for(runtime_dir.path(), &current_config)
        else {
            eyre::bail!("expected unix endpoint");
        };
        let listener = tokio::net::UnixListener::bind(&direct_path)?;
        let mismatched_response = serde_json::to_vec(&serde_json::json!({
            "Description": {
                "protocol_version": { "major": 2, "minor": 0 },
                "id": endpoint_hash(&current_config),
                "pid": 42,
                "start_time": 99,
                "name": "old-root",
                "working_dir": project_dir.path(),
                "config_path": current_config,
                "services": [],
                "micromux_version": "old"
            }
        }))?;
        let mismatched_socket = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream.write_all(&mismatched_response).await;
                let _ = stream.write_all(b"\n").await;
                let _ = stream.flush().await;
            }
        });

        let alias_endpoint = ControlEndpoint::Unix(runtime_dir.path().join("tools-session.sock"));
        let session = boot_at_endpoint_with_working_dir(
            &alias_endpoint,
            "live",
            &live_config,
            project_dir.path(),
        )?;
        let runtime_dirs = vec![runtime_dir.path().to_path_buf()];
        let dir_statuses = runtime_dirs
            .iter()
            .cloned()
            .map(|path| RuntimeDirStatus {
                path,
                usable: true,
                error: None,
            })
            .collect::<Vec<_>>();

        let resolved =
            resolve_in_dirs(&runtime_dirs, &dir_statuses, project_dir.path(), None).await;
        let Err(ToolError::Busy(message)) = resolved else {
            eyre::bail!("expected protocol mismatch to block fallback");
        };
        assert!(message.contains("the current config"));
        assert!(message.contains("protocol mismatch"));

        mismatched_socket.abort();
        session.shutdown.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn current_selector_does_not_retarget_to_subconfig_when_current_config_is_busy()
    -> eyre::Result<()> {
        let runtime_dir = temp_dir("current-busy-no-subconfig-fallback")?;
        let project_dir = temp_dir("current-busy-project")?;
        std::fs::create_dir(project_dir.path().join("tools"))?;
        std::fs::write(
            project_dir.path().join("micromux.yaml"),
            "version: 1\nservices:\n  stale:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
        )?;
        std::fs::write(
            project_dir.path().join("tools/micromux.yaml"),
            "version: 1\nservices:\n  live:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
        )?;
        let current_config = std::fs::canonicalize(project_dir.path().join("micromux.yaml"))?;
        let live_config = std::fs::canonicalize(project_dir.path().join("tools/micromux.yaml"))?;

        let ControlEndpoint::Unix(direct_path) = endpoint_for(runtime_dir.path(), &current_config)
        else {
            eyre::bail!("expected unix endpoint");
        };
        let listener = tokio::net::UnixListener::bind(&direct_path)?;
        let garbage = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream.write_all(b"not json\n").await;
                let _ = stream.flush().await;
            }
        });

        let alias_endpoint = ControlEndpoint::Unix(runtime_dir.path().join("tools-session.sock"));
        let session = boot_at_endpoint_with_working_dir(
            &alias_endpoint,
            "live",
            &live_config,
            project_dir.path(),
        )?;
        let runtime_dirs = vec![runtime_dir.path().to_path_buf()];
        let dir_statuses = runtime_dirs
            .iter()
            .cloned()
            .map(|path| RuntimeDirStatus {
                path,
                usable: true,
                error: None,
            })
            .collect::<Vec<_>>();

        let resolved =
            resolve_in_dirs(&runtime_dirs, &dir_statuses, project_dir.path(), None).await;
        let Err(ToolError::Busy(message)) = resolved else {
            eyre::bail!("expected busy current config");
        };
        assert!(message.contains("the current config"));

        garbage.abort();
        session.shutdown.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn current_selector_reports_ambiguous_same_config_across_runtime_dirs() -> eyre::Result<()>
    {
        let first_runtime = temp_dir("current-ambiguous-first")?;
        let second_runtime = temp_dir("current-ambiguous-second")?;
        let project_dir = temp_dir("current-ambiguous-project")?;
        std::fs::write(
            project_dir.path().join("micromux.yaml"),
            "version: 1\nservices:\n  svc:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
        )?;
        let config_path = std::fs::canonicalize(project_dir.path().join("micromux.yaml"))?;
        let first = boot(first_runtime.path(), "first", &config_path)?;
        let second = boot(second_runtime.path(), "second", &config_path)?;

        let runtime_dirs = vec![
            first_runtime.path().to_path_buf(),
            second_runtime.path().to_path_buf(),
        ];
        let dir_statuses = runtime_dirs
            .iter()
            .cloned()
            .map(|path| RuntimeDirStatus {
                path,
                usable: true,
                error: None,
            })
            .collect::<Vec<_>>();
        let resolved =
            resolve_in_dirs(&runtime_dirs, &dir_statuses, project_dir.path(), None).await;
        let Err(ToolError::Ambiguous(message)) = resolved else {
            eyre::bail!("expected ambiguous current session");
        };
        assert_eq!(
            message,
            "the current config matched 2 live sessions; use pid: or name: to disambiguate"
        );

        first.shutdown.cancel();
        second.shutdown.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn current_selector_dedupes_aliases_to_the_same_session() -> eyre::Result<()> {
        let runtime_dir = temp_dir("current-aliased")?;
        let project_dir = temp_dir("current-aliased-project")?;
        std::fs::write(
            project_dir.path().join("micromux.yaml"),
            "version: 1\nservices:\n  svc:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
        )?;
        let config_path = std::fs::canonicalize(project_dir.path().join("micromux.yaml"))?;
        let session = boot(runtime_dir.path(), "live", &config_path)?;

        let runtime_dirs = vec![
            runtime_dir.path().to_path_buf(),
            runtime_dir.path().to_path_buf(),
        ];
        let dir_statuses = runtime_dirs
            .iter()
            .cloned()
            .map(|path| RuntimeDirStatus {
                path,
                usable: true,
                error: None,
            })
            .collect::<Vec<_>>();
        let resolved =
            resolve_in_dirs(&runtime_dirs, &dir_statuses, project_dir.path(), None).await?;
        assert_eq!(resolved.info.name, "live");

        let by_name = resolve_in_dirs(
            &runtime_dirs,
            &dir_statuses,
            project_dir.path(),
            Some("name:live".to_string()),
        )
        .await?;
        assert_eq!(by_name.info.name, "live");

        let by_pid = resolve_in_dirs(
            &runtime_dirs,
            &dir_statuses,
            project_dir.path(),
            Some(format!("pid:{}", resolved.info.pid)),
        )
        .await?;
        assert_eq!(by_pid.info.name, "live");

        session.shutdown.cancel();
        Ok(())
    }

    #[test]
    fn session_listing_dedupes_aliased_sessions() {
        fn info(id: &str, name: &str, pid: u32, start_time: u64) -> SessionInfo {
            SessionInfo {
                protocol_version: micromux_control::PROTOCOL_VERSION,
                id: id.to_string(),
                pid,
                start_time,
                name: name.to_string(),
                working_dir: ".".to_string(),
                config_path: format!("/{name}/micromux.yaml"),
                services: vec![micromux_control::ServiceBrief {
                    id: "svc".into(),
                    name: "svc".to_string(),
                }],
                micromux_version: env!("CARGO_PKG_VERSION").to_string(),
                capabilities: None,
            }
        }

        let first = info("aaa", "live", 11, 22);
        let duplicate = first.clone();
        let second = info("bbb", "other", 33, 44);
        let probes = vec![
            EndpointProbe {
                endpoint: ControlEndpoint::Unix(PathBuf::from("/a.sock")),
                result: EndpointProbeResult::Session(Box::new(first)),
            },
            EndpointProbe {
                endpoint: ControlEndpoint::Unix(PathBuf::from("/alias.sock")),
                result: EndpointProbeResult::Session(Box::new(duplicate)),
            },
            EndpointProbe {
                endpoint: ControlEndpoint::Unix(PathBuf::from("/b.sock")),
                result: EndpointProbeResult::Session(Box::new(second)),
            },
        ];

        let sessions = session_infos_from_probes(&probes);
        assert_eq!(sessions.len(), 2);
        let names = sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["live", "other"]);
    }

    #[test]
    fn session_config_match_uses_identity_hash_not_display_path() {
        let config_path = PathBuf::from("/workspace/micromux.yaml");
        let info = SessionInfo {
            protocol_version: micromux_control::PROTOCOL_VERSION,
            id: endpoint_hash(&config_path),
            pid: 42,
            start_time: 99,
            name: "live".to_string(),
            working_dir: "/workspace".to_string(),
            config_path: "/different/display/path.yaml".to_string(),
            services: Vec::new(),
            micromux_version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities: None,
        };

        assert!(session_matches_config_path(&info, &config_path));
    }

    #[test]
    fn session_config_path_match_accepts_current_dir_descendant_configs() -> eyre::Result<()> {
        let project_dir = tempfile::tempdir()?;
        let nested_dir = project_dir.path().join("tools");
        std::fs::create_dir(&nested_dir)?;
        let info = SessionInfo {
            protocol_version: micromux_control::PROTOCOL_VERSION,
            id: "live".to_string(),
            pid: 42,
            start_time: 99,
            name: "live".to_string(),
            working_dir: project_dir.path().display().to_string(),
            config_path: project_dir
                .path()
                .join("tools/micromux.yaml")
                .display()
                .to_string(),
            services: Vec::new(),
            micromux_version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities: None,
        };

        assert!(session_config_path_is_under(&info, project_dir.path()));
        assert!(session_config_path_is_under(&info, &nested_dir));
        Ok(())
    }

    #[test]
    fn probe_detail_surfaces_protocol_mismatch_versions() {
        let peer = ProtocolVersion::new(2, 0);
        let ours = ProtocolVersion::new(1, 0);
        let detail = probe_detail(EndpointProbe {
            endpoint: ControlEndpoint::Unix(PathBuf::from("/newer.sock")),
            result: EndpointProbeResult::ProtocolMismatch { peer, ours },
        });

        assert!(matches!(detail.status, SocketProbeStatus::ProtocolMismatch));
        assert_eq!(detail.peer_protocol_version, Some(peer));
        assert_eq!(detail.client_protocol_version, Some(ours));
        assert!(detail.session.is_none());
        assert_eq!(
            detail.message.as_deref(),
            Some("control protocol version mismatch: peer=2.0, ours=1.0"),
        );
    }

    #[tokio::test]
    async fn already_running_reports_a_live_session_and_skips_a_dead_endpoint() -> eyre::Result<()>
    {
        let runtime_dir = temp_dir("already-running")?;
        let config_path = runtime_dir.path().join("proj/micromux.yaml");
        let session = boot(runtime_dir.path(), "live", &config_path)?;

        // A live session makes start_session a no-op.
        let endpoint = endpoint_for(runtime_dir.path(), &config_path);
        let endpoints = vec![endpoint];
        assert!(
            crate::already_running_any(&endpoints, &config_path)
                .await?
                .is_some(),
            "a live session must be reported as already running"
        );

        let garbage = ControlEndpoint::Unix(runtime_dir.path().join("garbage.sock"));
        let ControlEndpoint::Unix(garbage_path) = &garbage else {
            eyre::bail!("expected unix endpoint");
        };
        let listener = tokio::net::UnixListener::bind(garbage_path)?;
        let garbage_task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream.write_all(b"not json\n").await;
                let _ = stream.flush().await;
            }
        });
        let endpoints = vec![garbage.clone()];
        assert!(
            crate::already_running_any(&endpoints, &config_path)
                .await?
                .is_none(),
            "an unparsable socket is not ownership proof; serve's lifetime lock decides"
        );

        let locked_garbage = ControlEndpoint::Unix(runtime_dir.path().join("locked-garbage.sock"));
        let ControlEndpoint::Unix(locked_garbage_path) = &locked_garbage else {
            eyre::bail!("expected unix endpoint");
        };
        let locked_listener = tokio::net::UnixListener::bind(locked_garbage_path)?;
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(locked_garbage_path.with_extension("lock"))?;
        lock_file.lock_exclusive()?;
        let locked_task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = locked_listener.accept().await {
                let _ = stream.write_all(b"not json\n").await;
                let _ = stream.flush().await;
            }
        });
        let endpoints = vec![locked_garbage.clone()];
        let locked_report = crate::already_running_any(&endpoints, &config_path)
            .await?
            .ok_or_else(|| eyre::eyre!("expected lock-held owner report"))?;
        assert_eq!(locked_report.reachable, Some(false));

        let endpoint = endpoint_for(runtime_dir.path(), &config_path);
        let endpoints = vec![garbage, endpoint];
        let report = crate::already_running_any(&endpoints, &config_path)
            .await?
            .ok_or_else(|| eyre::eyre!("expected existing session report"))?;
        assert_eq!(report.session.as_deref(), Some("live"));

        // A project with no listener is free to start.
        let dead = endpoint_for(
            runtime_dir.path(),
            &runtime_dir.path().join("absent/micromux.yaml"),
        );
        let endpoints = vec![dead];
        assert!(
            crate::already_running_any(&endpoints, &config_path)
                .await?
                .is_none()
        );

        locked_task.abort();
        garbage_task.abort();
        session.shutdown.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn already_running_reports_ambiguous_distinct_sessions() -> eyre::Result<()> {
        let first_runtime = temp_dir("already-running-ambiguous-first")?;
        let second_runtime = temp_dir("already-running-ambiguous-second")?;
        let project_dir = temp_dir("already-running-ambiguous-project")?;
        let config_path = project_dir.path().join("micromux.yaml");
        let first = boot(first_runtime.path(), "first", &config_path)?;
        let second = boot(second_runtime.path(), "second", &config_path)?;
        let endpoints = vec![
            endpoint_for(first_runtime.path(), &config_path),
            endpoint_for(second_runtime.path(), &config_path),
        ];

        let report = crate::already_running_any(&endpoints, &config_path).await;
        assert!(matches!(report, Err(ToolError::Ambiguous(_))));

        first.shutdown.cancel();
        second.shutdown.cancel();
        Ok(())
    }
}
