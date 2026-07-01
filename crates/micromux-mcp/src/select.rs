//! Read-only, connect-to-verify session selection.
//!
//! The proxy prepares candidate runtime directories but never prunes endpoints or mutates sessions.
//! A typed selector resolves to exactly one live session, or a typed error
//! (`NoSession`/`Ambiguous`/`Unsupported`).

use std::path::{Path, PathBuf};

use micromux_control::{
    ControlEndpoint, ControlError, EndpointProbe, EndpointProbeResult, RuntimeDirStatus,
    SessionInfo, endpoint_for, endpoint_from_hash, probe_endpoints, probe_runtime_dirs,
    runtime_dir_statuses, unique_answering_session_probes, usable_runtime_dirs,
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
pub struct RuntimeDirDetail {
    pub path: String,
    pub usable: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SocketProbeDetail {
    pub endpoint: String,
    pub status: SocketProbeStatus,
    pub message: Option<String>,
    pub session: Option<SessionInfo>,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SocketProbeStatus {
    Session,
    Absent,
    Unreachable,
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

/// A resolved session: the endpoint to drive and its live identity.
pub struct Resolved {
    /// The control endpoint to connect to.
    pub endpoint: ControlEndpoint,
    /// The session's live identity (carries `config_path` so the target is legible).
    pub info: SessionInfo,
}

enum Selector {
    Current,
    Name(String),
    Pid(u32),
    Hash(String),
}

fn parse_selector(raw: Option<String>) -> Selector {
    let raw = raw.or_else(|| std::env::var("MICROMUX_SESSION").ok());
    let Some(raw) = raw else {
        return Selector::Current;
    };
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("current") {
        Selector::Current
    } else if let Some(pid) = raw.strip_prefix("pid:") {
        pid.trim()
            .parse()
            .map_or_else(|_| Selector::Name(raw.to_string()), Selector::Pid)
    } else if let Some(hash) = raw.strip_prefix("hash:") {
        Selector::Hash(hash.trim().to_string())
    } else if let Some(name) = raw.strip_prefix("name:") {
        Selector::Name(name.trim().to_string())
    } else {
        Selector::Name(raw.to_string())
    }
}

/// Resolve the config path for a start target: an existing config *file* is used directly; a
/// directory (or the default cwd) is searched upward. Returns the canonical config path.
pub(crate) async fn config_for_target(path: &Path) -> Option<PathBuf> {
    if tokio::fs::metadata(path)
        .await
        .is_ok_and(|meta| meta.is_file())
    {
        return tokio::fs::canonicalize(path).await.ok();
    }
    find_config_upward(path).await
}

/// Walk upward from `start`, returning the first canonicalized micromux config path found.
///
/// Reimplemented here (rather than calling `micromux::find_config_file`) so the future does not
/// capture that function's `|name| dir.join(name)` closure, which the rmcp `#[tool]` macro's
/// higher-ranked `'static` bound on the tool future rejects.
pub(crate) async fn find_config_upward(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        for name in micromux::config_file_names() {
            let candidate = dir.join(name);
            if let Ok(canonical) = tokio::fs::canonicalize(&candidate).await {
                return Some(canonical);
            }
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return None,
        }
    }
}

async fn verify_any(
    endpoints: Vec<ControlEndpoint>,
    describe: &str,
    absent: impl Fn(Vec<SocketProbeDetail>) -> DiscoveryDiagnostics,
) -> Result<Resolved, ToolError> {
    let probes = probe_endpoints(&endpoints).await;
    let matches = unique_answering_session_probes(&probes)
        .into_iter()
        .map(|(endpoint, info)| Resolved { endpoint, info })
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        return Err(ToolError::Ambiguous(format!(
            "{describe} matched {} live sessions; use pid: or name: to disambiguate",
            matches.len()
        )));
    }
    if let Some(resolved) = matches.into_iter().next() {
        return Ok(resolved);
    }

    let unreachable = unreachable_messages(&probes);
    if !unreachable.is_empty() {
        return Err(ToolError::Busy(format!(
            "no answering session matched {describe}; {} live endpoint(s) were unreachable and may be the target: {}",
            unreachable.len(),
            unreachable.join("; ")
        )));
    }
    Err(ToolError::NoSession(Box::new(absent(
        probes.into_iter().map(probe_detail).collect(),
    ))))
}

/// Resolve a selector to a single live session.
///
/// # Errors
///
/// Returns [`ToolError::NoSession`] when nothing answers, [`ToolError::Ambiguous`] when a name
/// matches more than one, or [`ToolError::Unsupported`] on a platform without a transport.
pub async fn resolve(cwd: &Path, selector: Option<String>) -> Result<Resolved, ToolError> {
    if !micromux_control::transport_supported() {
        return Err(ToolError::Unsupported);
    }
    let dir_statuses = runtime_dir_statuses();
    let runtime_dirs = usable_runtime_dirs(&dir_statuses);
    if runtime_dirs.is_empty() {
        return Err(ToolError::NoSession(Box::new(discovery_diagnostics(
            cwd,
            "no runtime directory could be resolved".to_string(),
            None,
            &dir_statuses,
            Vec::new(),
        ))));
    }
    resolve_in_dirs(&runtime_dirs, &dir_statuses, cwd, selector).await
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
    let dir_statuses = runtime_dirs
        .iter()
        .cloned()
        .map(|path| RuntimeDirStatus {
            path,
            usable: true,
            error: None,
        })
        .collect::<Vec<_>>();
    resolve_in_dirs(&runtime_dirs, &dir_statuses, cwd, selector).await
}

async fn resolve_in_dirs(
    runtime_dirs: &[PathBuf],
    dir_statuses: &[RuntimeDirStatus],
    cwd: &Path,
    selector: Option<String>,
) -> Result<Resolved, ToolError> {
    match parse_selector(selector) {
        Selector::Current => resolve_current(runtime_dirs, dir_statuses, cwd).await,
        Selector::Hash(hash) => resolve_hash(runtime_dirs, dir_statuses, cwd, &hash).await,
        Selector::Name(name) => resolve_name(runtime_dirs, dir_statuses, cwd, &name).await,
        Selector::Pid(pid) => resolve_pid(runtime_dirs, dir_statuses, cwd, pid).await,
    }
}

async fn resolve_current(
    runtime_dirs: &[PathBuf],
    dir_statuses: &[RuntimeDirStatus],
    cwd: &Path,
) -> Result<Resolved, ToolError> {
    let config_path = find_config_upward(cwd).await.ok_or_else(|| {
        ToolError::NoSession(Box::new(discovery_diagnostics(
            cwd,
            "no micromux config found from the current directory".to_string(),
            None,
            dir_statuses,
            Vec::new(),
        )))
    })?;
    let endpoints = runtime_dirs
        .iter()
        .map(|runtime_dir| endpoint_for(runtime_dir, &config_path))
        .collect();
    verify_any(
        endpoints,
        "the current config",
        |socket_probes| {
            let dir = config_path.parent().unwrap_or(config_path.as_path());
            discovery_diagnostics(
                cwd,
                format!(
                    "no running micromux session for {}; start one with the start_session tool, or run `micromux` in {}",
                    config_path.display(),
                    dir.display(),
                ),
                Some(&config_path),
                dir_statuses,
                socket_probes,
            )
        },
    )
    .await
}

async fn resolve_hash(
    runtime_dirs: &[PathBuf],
    dir_statuses: &[RuntimeDirStatus],
    cwd: &Path,
    hash: &str,
) -> Result<Resolved, ToolError> {
    let endpoints = runtime_dirs
        .iter()
        .map(|runtime_dir| endpoint_from_hash(runtime_dir, hash))
        .collect();
    let describe = format!("hash {hash}");
    verify_any(endpoints, &describe, |socket_probes| {
        discovery_diagnostics(
            cwd,
            format!("no session with hash {hash}"),
            None,
            dir_statuses,
            socket_probes,
        )
    })
    .await
}

async fn resolve_name(
    runtime_dirs: &[PathBuf],
    dir_statuses: &[RuntimeDirStatus],
    cwd: &Path,
    name: &str,
) -> Result<Resolved, ToolError> {
    resolve_scanned_selector(
        runtime_dirs,
        dir_statuses,
        cwd,
        &format!("name `{name}`"),
        "use pid: or hash: to disambiguate",
        |info| info.name == name,
    )
    .await
}

async fn resolve_pid(
    runtime_dirs: &[PathBuf],
    dir_statuses: &[RuntimeDirStatus],
    cwd: &Path,
    pid: u32,
) -> Result<Resolved, ToolError> {
    resolve_scanned_selector(
        runtime_dirs,
        dir_statuses,
        cwd,
        &format!("pid {pid}"),
        "use hash: to disambiguate",
        |info| info.pid == pid,
    )
    .await
}

async fn resolve_scanned_selector(
    runtime_dirs: &[PathBuf],
    dir_statuses: &[RuntimeDirStatus],
    cwd: &Path,
    describe: &str,
    disambiguate_hint: &str,
    matches_session: impl Fn(&SessionInfo) -> bool,
) -> Result<Resolved, ToolError> {
    let probes = probe_runtime_dirs(runtime_dirs)
        .await
        .map_err(ToolError::from)?;
    let matches = unique_answering_session_probes(&probes)
        .into_iter()
        .filter(|(_endpoint, info)| matches_session(info))
        .map(|(endpoint, info)| Resolved { endpoint, info })
        .collect();
    resolve_probe_matches(
        matches,
        probes,
        dir_statuses,
        cwd,
        describe,
        disambiguate_hint,
    )
}

fn resolve_probe_matches(
    matches: Vec<Resolved>,
    probes: Vec<EndpointProbe>,
    dir_statuses: &[RuntimeDirStatus],
    cwd: &Path,
    describe: &str,
    disambiguate_hint: &str,
) -> Result<Resolved, ToolError> {
    if matches.is_empty() {
        let unreachable = unreachable_messages(&probes);
        if !unreachable.is_empty() {
            return Err(ToolError::Busy(format!(
                "no answering session matched {describe}; {} live session(s) were unreachable and may be the target: {}",
                unreachable.len(),
                unreachable.join("; ")
            )));
        }
        return Err(ToolError::NoSession(Box::new(discovery_diagnostics(
            cwd,
            format!("no session matched {describe}"),
            None,
            dir_statuses,
            probes.into_iter().map(probe_detail).collect(),
        ))));
    }
    if matches.len() > 1 {
        return Err(ToolError::Ambiguous(format!(
            "{describe} matched {} live sessions; {disambiguate_hint}",
            matches.len()
        )));
    }
    let mut matches = matches.into_iter();
    let Some(resolved) = matches.next() else {
        return Err(ToolError::Unexpected(
            "selector invariant violated: exactly one match expected".to_string(),
        ));
    };
    Ok(resolved)
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

fn probe_detail(probe: EndpointProbe) -> SocketProbeDetail {
    match probe.result {
        EndpointProbeResult::Session(info) => SocketProbeDetail {
            endpoint: probe.endpoint.to_string(),
            status: SocketProbeStatus::Session,
            message: None,
            session: Some(*info),
        },
        EndpointProbeResult::Absent(reason) => SocketProbeDetail {
            endpoint: probe.endpoint.to_string(),
            status: SocketProbeStatus::Absent,
            message: Some(reason),
            session: None,
        },
        EndpointProbeResult::Unreachable(reason) => SocketProbeDetail {
            endpoint: probe.endpoint.to_string(),
            status: SocketProbeStatus::Unreachable,
            message: Some(reason),
            session: None,
        },
    }
}

fn unreachable_messages(probes: &[EndpointProbe]) -> Vec<String> {
    probes
        .iter()
        .filter_map(|probe| match &probe.result {
            EndpointProbeResult::Unreachable(reason) => {
                Some(format!("{} -> {reason}", probe.endpoint))
            }
            EndpointProbeResult::Session(_) | EndpointProbeResult::Absent(_) => None,
        })
        .collect()
}

fn session_infos_from_probes(probes: &[EndpointProbe]) -> Vec<SessionInfo> {
    unique_answering_session_probes(probes)
        .into_iter()
        .map(|(_endpoint, info)| info)
        .collect()
}

/// List every session that answers `Describe`. Endpoints that refuse a connection are skipped;
/// live-but-unreachable ones are simply not listed this scan (never pruned).
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
    let socket_probes = probes.into_iter().map(probe_detail).collect();
    Ok(SessionList {
        sessions,
        diagnostics: discovery_diagnostics(
            &cwd,
            "session discovery".to_string(),
            None,
            &dir_statuses,
            socket_probes,
        ),
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Arc;

    use fs2::FileExt;
    use micromux::CancellationToken;
    use micromux_control::{ControlEndpoint, ControlServer, SessionIdentity, bind, endpoint_for};
    use similar_asserts::assert_eq;
    use tokio::io::AsyncWriteExt;

    struct Running {
        shutdown: CancellationToken,
        _runner: tokio::task::JoinHandle<color_eyre::eyre::Result<()>>,
    }

    fn temp_dir(prefix: &str) -> color_eyre::eyre::Result<tempfile::TempDir> {
        Ok(tempfile::Builder::new()
            .prefix(&format!("micromux-mcp-{prefix}-"))
            .tempdir()?)
    }

    fn boot(
        runtime_dir: &Path,
        name: &str,
        config_path: &Path,
    ) -> color_eyre::eyre::Result<Running> {
        let yaml = "version: 1\nservices:\n  svc:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n";
        let mut diagnostics = vec![];
        let config = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)
            .map_err(|err| color_eyre::eyre::eyre!("parse: {err}"))?;
        let mux = Arc::new(micromux::Micromux::new(&config)?);
        let shutdown = CancellationToken::new();
        let (runner, handles) = mux.clone().start(shutdown.clone());
        let runner = tokio::spawn(runner);

        let endpoint = endpoint_for(runtime_dir, config_path);
        let guard = bind(&endpoint)?.ok_or_else(|| color_eyre::eyre::eyre!("bind failed"))?;
        let identity = SessionIdentity::new(name.to_string(), Path::new("."), config_path);
        let server = Arc::new(ControlServer::new(
            handles.reader.clone(),
            handles.service_control(),
            identity,
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
    async fn resolves_by_name_skips_dead_and_detects_ambiguity() -> color_eyre::eyre::Result<()> {
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
    -> color_eyre::eyre::Result<()> {
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
            color_eyre::eyre::bail!("expected unix endpoint");
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
    async fn current_selector_reports_ambiguous_same_config_across_runtime_dirs()
    -> color_eyre::eyre::Result<()> {
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
        assert!(matches!(resolved, Err(ToolError::Ambiguous(_))));

        first.shutdown.cancel();
        second.shutdown.cancel();
        Ok(())
    }

    #[tokio::test]
    async fn current_selector_dedupes_aliases_to_the_same_session() -> color_eyre::eyre::Result<()>
    {
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

    #[tokio::test]
    async fn already_running_reports_a_live_session_and_skips_a_dead_endpoint()
    -> color_eyre::eyre::Result<()> {
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
            color_eyre::eyre::bail!("expected unix endpoint");
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
            "an unparseable socket is not ownership proof; serve's lifetime lock decides"
        );

        let locked_garbage = ControlEndpoint::Unix(runtime_dir.path().join("locked-garbage.sock"));
        let ControlEndpoint::Unix(locked_garbage_path) = &locked_garbage else {
            color_eyre::eyre::bail!("expected unix endpoint");
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
            .ok_or_else(|| color_eyre::eyre::eyre!("expected lock-held owner report"))?;
        assert_eq!(locked_report.reachable, Some(false));

        let endpoint = endpoint_for(runtime_dir.path(), &config_path);
        let endpoints = vec![garbage, endpoint];
        let report = crate::already_running_any(&endpoints, &config_path)
            .await?
            .ok_or_else(|| color_eyre::eyre::eyre!("expected existing session report"))?;
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
    async fn already_running_reports_ambiguous_distinct_sessions() -> color_eyre::eyre::Result<()> {
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
