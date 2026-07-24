//! Session selection from live control endpoints.

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Serialize;

use crate::{
    ControlEndpoint, ControlError, EndpointProbe, EndpointProbeResult, PROTOCOL_VERSION,
    ProtocolVersion, RuntimeDirStatus, SessionInfo, endpoint_for, endpoint_from_hash,
    endpoint_hash, probe_endpoints, probe_runtime_dirs, runtime_dir_statuses,
    unique_answering_session_probes, usable_runtime_dirs,
};

/// A session selector accepted by the control-plane resolver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSelector {
    /// Resolve the config selected by the current directory or an explicit config override.
    Current,
    /// Match the session's configured display name.
    Name(String),
    /// Match the session process id.
    Pid(u32),
    /// Match the deterministic endpoint hash for a config path.
    ConfigHash(String),
}

impl SessionSelector {
    /// Parse selector syntax without rejecting malformed prefixed values.
    ///
    /// Invalid prefixed forms such as `pid:x` remain usable as literal session names.
    #[must_use]
    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim();
        if raw.is_empty() || raw.eq_ignore_ascii_case("current") {
            Self::Current
        } else if let Some(pid) = raw.strip_prefix("pid:") {
            pid.trim()
                .parse()
                .map_or_else(|_| Self::Name(raw.to_string()), Self::Pid)
        } else if let Some(hash) = raw.strip_prefix("hash:") {
            Self::ConfigHash(hash.trim().to_string())
        } else if let Some(name) = raw.strip_prefix("name:") {
            Self::Name(name.trim().to_string())
        } else {
            Self::Name(raw.to_string())
        }
    }
}

/// Usability details for one candidate runtime directory.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RuntimeDirDetail {
    /// Runtime directory path.
    pub path: String,
    /// Whether the directory can host or discover control endpoints.
    pub usable: bool,
    /// Why the directory is unusable.
    pub error: Option<String>,
}

/// Probe details for one derived or discovered control endpoint.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SocketProbeDetail {
    /// Display form of the endpoint.
    pub endpoint: String,
    /// Probe classification.
    pub status: SocketProbeStatus,
    /// Transport or compatibility detail.
    pub message: Option<String>,
    /// Protocol version advertised by the session, when one was decoded.
    pub peer_protocol_version: Option<ProtocolVersion>,
    /// Protocol version spoken by this client.
    pub client_protocol_version: Option<ProtocolVersion>,
    /// Live session identity when the endpoint answered successfully.
    pub session: Option<SessionInfo>,
}

/// Result class for a control endpoint probe.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SocketProbeStatus {
    /// The endpoint answered with a compatible session description.
    Session,
    /// Nothing live answered at the endpoint.
    Absent,
    /// The endpoint answered with an incompatible protocol version.
    ProtocolMismatch,
    /// Something was listening but could not answer a usable request.
    Unreachable,
}

/// Structured transport facts collected while resolving a selector.
#[derive(Debug, Clone)]
pub struct ProbeReport {
    /// Human summary of the failed resolution.
    pub summary: String,
    /// Canonical config path when resolution reached one.
    pub config_path: Option<String>,
    /// Candidate runtime directories.
    pub runtime_dirs: Vec<RuntimeDirDetail>,
    /// Probe result for each candidate endpoint.
    pub socket_probes: Vec<SocketProbeDetail>,
}

/// A live session selected together with the endpoint that drives it.
pub struct ResolvedSession {
    /// The control endpoint to connect to.
    pub endpoint: ControlEndpoint,
    /// Live session identity returned by the endpoint.
    pub info: SessionInfo,
}

/// Details for a selector that matched more than one live session.
#[derive(Debug)]
pub struct AmbiguousSelection {
    /// Human-readable explanation and disambiguation hint.
    pub message: String,
    /// The live candidates, used by interactive clients to print concrete selectors.
    pub candidates: Vec<SessionInfo>,
}

/// Failures produced while selecting a live session.
#[derive(Debug, thiserror::Error)]
pub enum SelectError {
    /// No live session matched.
    #[error("no session: {}", .0.summary)]
    NoSession(Box<ProbeReport>),
    /// More than one live session matched.
    #[error("ambiguous selector: {}", .0.message)]
    Ambiguous(Box<AmbiguousSelection>),
    /// A possibly matching endpoint existed but was unusable.
    #[error("session busy: {0}")]
    Busy(String),
    /// The control transport is unavailable on this platform.
    #[error("the micromux control plane is not supported on this platform")]
    Unsupported,
    /// Endpoint discovery or probing failed.
    #[error(transparent)]
    Control(ControlError),
}

impl From<ControlError> for SelectError {
    fn from(error: ControlError) -> Self {
        match error {
            ControlError::Unsupported => Self::Unsupported,
            ControlError::Timeout => {
                Self::Busy("the micromux session did not answer in time".to_string())
            }
            other => Self::Control(other),
        }
    }
}

/// Resolve a selector to exactly one live session across all usable runtime directories.
///
/// `None` selects [`SessionSelector::Current`]. A config override participates only in current
/// selection; explicit name, pid, and hash selectors ignore it.
///
/// # Errors
///
/// Returns [`SelectError::NoSession`] when nothing answers, [`SelectError::Ambiguous`] when more
/// than one session matches, or [`SelectError::Unsupported`] without a control transport.
pub async fn resolve_selector(
    cwd: &Path,
    selector: Option<SessionSelector>,
    config_override: Option<&Path>,
) -> Result<ResolvedSession, SelectError> {
    if !crate::transport_supported() {
        return Err(SelectError::Unsupported);
    }
    let dir_statuses = runtime_dir_statuses();
    let runtime_dirs = usable_runtime_dirs(&dir_statuses);
    if runtime_dirs.is_empty() {
        return Err(SelectError::NoSession(Box::new(probe_report(
            "no runtime directory could be resolved".to_string(),
            None,
            &dir_statuses,
            Vec::new(),
        ))));
    }
    resolve_selector_in_runtime_dirs(&runtime_dirs, &dir_statuses, cwd, selector, config_override)
        .await
}

/// Resolve a selector against caller-supplied runtime directories.
///
/// This lower-level form supports clients that have already prepared or constrained discovery.
///
/// # Errors
///
/// Returns the same errors as [`resolve_selector`].
pub async fn resolve_selector_in_runtime_dirs(
    runtime_dirs: &[PathBuf],
    dir_statuses: &[RuntimeDirStatus],
    cwd: &Path,
    selector: Option<SessionSelector>,
    config_override: Option<&Path>,
) -> Result<ResolvedSession, SelectError> {
    match selector.unwrap_or(SessionSelector::Current) {
        SessionSelector::Current => {
            resolve_current(runtime_dirs, dir_statuses, cwd, config_override).await
        }
        SessionSelector::ConfigHash(hash) => resolve_hash(runtime_dirs, dir_statuses, &hash).await,
        SessionSelector::Name(name) => resolve_name(runtime_dirs, dir_statuses, &name).await,
        SessionSelector::Pid(pid) => resolve_pid(runtime_dirs, dir_statuses, pid).await,
    }
}

async fn verify_any(
    endpoints: Vec<ControlEndpoint>,
    describe: &str,
    absent: impl Fn(Vec<SocketProbeDetail>) -> ProbeReport,
) -> Result<ResolvedSession, SelectError> {
    let probes = probe_endpoints(&endpoints).await;
    let matches = unique_answering_session_probes(&probes)
        .into_iter()
        .map(|(endpoint, info)| ResolvedSession { endpoint, info })
        .collect::<Vec<_>>();
    classify_probe_matches(
        matches,
        probes,
        describe,
        "use pid: or name: to disambiguate",
        "endpoint(s)",
        absent,
    )
}

async fn resolve_current(
    runtime_dirs: &[PathBuf],
    dir_statuses: &[RuntimeDirStatus],
    cwd: &Path,
    config_override: Option<&Path>,
) -> Result<ResolvedSession, SelectError> {
    let target = config_override.unwrap_or(cwd);
    let config_path = config_for_target(target).await.ok_or_else(|| {
        SelectError::NoSession(Box::new(probe_report(
            if config_override.is_some() {
                format!("no micromux config found at or above {}", target.display())
            } else {
                "no micromux config found from the current directory".to_string()
            },
            None,
            dir_statuses,
            Vec::new(),
        )))
    })?;
    let endpoints = runtime_dirs
        .iter()
        .map(|runtime_dir| endpoint_for(runtime_dir, &config_path))
        .collect();
    match verify_any(endpoints, "the current config", |socket_probes| {
        let dir = config_path.parent().unwrap_or(config_path.as_path());
        probe_report(
            format!(
                "no running micromux session for {}; start one with the start_session tool, or run `micromux` in {}",
                config_path.display(),
                dir.display(),
            ),
            Some(&config_path),
            dir_statuses,
            socket_probes,
        )
    })
    .await
    {
        Ok(resolved) => Ok(resolved),
        Err(error @ SelectError::Busy(_)) => {
            match resolve_current_config_by_scanning(runtime_dirs, &config_path).await? {
                Some(resolved) => Ok(resolved),
                None => Err(error),
            }
        }
        Err(error @ SelectError::NoSession(_)) => {
            let scanned = if config_override.is_some() {
                resolve_current_config_by_scanning(runtime_dirs, &config_path).await?
            } else {
                resolve_current_project_by_scanning(runtime_dirs, cwd, &config_path).await?
            };
            match scanned {
                Some(resolved) => Ok(resolved),
                None => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

async fn resolve_current_config_by_scanning(
    runtime_dirs: &[PathBuf],
    config_path: &Path,
) -> Result<Option<ResolvedSession>, SelectError> {
    let probes = probe_runtime_dirs(runtime_dirs).await?;
    resolve_current_config_matches(&probes, config_path)
}

async fn resolve_current_project_by_scanning(
    runtime_dirs: &[PathBuf],
    cwd: &Path,
    config_path: &Path,
) -> Result<Option<ResolvedSession>, SelectError> {
    let probes = probe_runtime_dirs(runtime_dirs).await?;
    if let Some(resolved) = resolve_current_config_matches(&probes, config_path)? {
        return Ok(Some(resolved));
    }
    let project_matches =
        current_scan_matches(&probes, |info| session_config_path_is_under(info, cwd));
    resolve_unique_current_scan_matches(project_matches, "a config under the current directory")
}

fn resolve_current_config_matches(
    probes: &[EndpointProbe],
    config_path: &Path,
) -> Result<Option<ResolvedSession>, SelectError> {
    let config_matches = current_scan_matches(probes, |info| {
        session_matches_config_path(info, config_path)
    });
    resolve_unique_current_scan_matches(config_matches, "the current config")
}

fn current_scan_matches(
    probes: &[EndpointProbe],
    matches_session: impl Fn(&SessionInfo) -> bool,
) -> Vec<ResolvedSession> {
    unique_answering_session_probes(probes)
        .into_iter()
        .filter(|(_endpoint, info)| matches_session(info))
        .map(|(endpoint, info)| ResolvedSession { endpoint, info })
        .collect()
}

fn resolve_unique_current_scan_matches(
    matches: Vec<ResolvedSession>,
    describe: &str,
) -> Result<Option<ResolvedSession>, SelectError> {
    if matches.len() > 1 {
        return Err(ambiguous_selection(
            format!(
                "{describe} matched {} live sessions; use pid: or name: to disambiguate",
                matches.len()
            ),
            &matches,
        ));
    }
    Ok(matches.into_iter().next())
}

/// Return whether a session's identity hash matches a canonical config path.
#[must_use]
pub fn session_matches_config_path(info: &SessionInfo, config_path: &Path) -> bool {
    info.id == endpoint_hash(config_path)
}

/// Return whether a session's canonical config path is inside a directory.
#[must_use]
pub fn session_config_path_is_under(info: &SessionInfo, directory: &Path) -> bool {
    if info.config_path.is_empty() {
        return false;
    }
    let session_config_path = canonicalize_for_match(Path::new(&info.config_path));
    let directory = canonicalize_for_match(directory);
    session_config_path.starts_with(&directory)
}

fn canonicalize_for_match(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

async fn resolve_hash(
    runtime_dirs: &[PathBuf],
    dir_statuses: &[RuntimeDirStatus],
    hash: &str,
) -> Result<ResolvedSession, SelectError> {
    let endpoints = runtime_dirs
        .iter()
        .map(|runtime_dir| endpoint_from_hash(runtime_dir, hash))
        .collect();
    let describe = format!("hash {hash}");
    verify_any(endpoints, &describe, |socket_probes| {
        probe_report(
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
    name: &str,
) -> Result<ResolvedSession, SelectError> {
    resolve_scanned_selector(
        runtime_dirs,
        dir_statuses,
        &format!("name `{name}`"),
        "use pid: or hash: to disambiguate",
        |info| info.name == name,
    )
    .await
}

async fn resolve_pid(
    runtime_dirs: &[PathBuf],
    dir_statuses: &[RuntimeDirStatus],
    pid: u32,
) -> Result<ResolvedSession, SelectError> {
    resolve_scanned_selector(
        runtime_dirs,
        dir_statuses,
        &format!("pid {pid}"),
        "use hash: to disambiguate",
        |info| info.pid == pid,
    )
    .await
}

async fn resolve_scanned_selector(
    runtime_dirs: &[PathBuf],
    dir_statuses: &[RuntimeDirStatus],
    describe: &str,
    disambiguate_hint: &str,
    matches_session: impl Fn(&SessionInfo) -> bool,
) -> Result<ResolvedSession, SelectError> {
    let probes = probe_runtime_dirs(runtime_dirs).await?;
    let matches = unique_answering_session_probes(&probes)
        .into_iter()
        .filter(|(_endpoint, info)| matches_session(info))
        .map(|(endpoint, info)| ResolvedSession { endpoint, info })
        .collect();
    classify_probe_matches(
        matches,
        probes,
        describe,
        disambiguate_hint,
        "session(s)",
        |socket_probes| {
            probe_report(
                format!("no session matched {describe}"),
                None,
                dir_statuses,
                socket_probes,
            )
        },
    )
}

fn classify_probe_matches(
    matches: Vec<ResolvedSession>,
    probes: Vec<EndpointProbe>,
    describe: &str,
    disambiguate_hint: &str,
    busy_unit: &str,
    report: impl FnOnce(Vec<SocketProbeDetail>) -> ProbeReport,
) -> Result<ResolvedSession, SelectError> {
    if matches.len() > 1 {
        return Err(ambiguous_selection(
            format!(
                "{describe} matched {} live sessions; {disambiguate_hint}",
                matches.len()
            ),
            &matches,
        ));
    }
    if let Some(resolved) = matches.into_iter().next() {
        return Ok(resolved);
    }
    let unusable = unusable_messages(&probes);
    if !unusable.is_empty() {
        return Err(SelectError::Busy(format!(
            "no answering session matched {describe}; {} live {busy_unit} were reachable but unusable and may be the target: {}",
            unusable.len(),
            unusable.join("; ")
        )));
    }
    Err(SelectError::NoSession(Box::new(report(
        probes.into_iter().map(probe_detail).collect(),
    ))))
}

fn ambiguous_selection(message: String, matches: &[ResolvedSession]) -> SelectError {
    SelectError::Ambiguous(Box::new(AmbiguousSelection {
        message,
        candidates: matches
            .iter()
            .map(|resolved| resolved.info.clone())
            .collect(),
    }))
}

/// Convert an endpoint probe into its transport-level diagnostic representation.
#[must_use]
pub fn probe_detail(probe: EndpointProbe) -> SocketProbeDetail {
    match probe.result {
        EndpointProbeResult::Session(info) => SocketProbeDetail {
            endpoint: probe.endpoint.to_string(),
            status: SocketProbeStatus::Session,
            message: None,
            peer_protocol_version: Some(info.protocol_version),
            client_protocol_version: Some(PROTOCOL_VERSION),
            session: Some(*info),
        },
        EndpointProbeResult::Absent(reason) => SocketProbeDetail {
            endpoint: probe.endpoint.to_string(),
            status: SocketProbeStatus::Absent,
            message: Some(reason),
            peer_protocol_version: None,
            client_protocol_version: Some(PROTOCOL_VERSION),
            session: None,
        },
        EndpointProbeResult::ProtocolMismatch { peer, ours } => SocketProbeDetail {
            endpoint: probe.endpoint.to_string(),
            status: SocketProbeStatus::ProtocolMismatch,
            message: Some(format!(
                "control protocol version mismatch: peer={peer}, ours={ours}"
            )),
            peer_protocol_version: Some(peer),
            client_protocol_version: Some(ours),
            session: None,
        },
        EndpointProbeResult::Unreachable(reason) => SocketProbeDetail {
            endpoint: probe.endpoint.to_string(),
            status: SocketProbeStatus::Unreachable,
            message: Some(reason),
            peer_protocol_version: None,
            client_protocol_version: Some(PROTOCOL_VERSION),
            session: None,
        },
    }
}

fn unusable_messages(probes: &[EndpointProbe]) -> Vec<String> {
    probes
        .iter()
        .filter_map(|probe| match &probe.result {
            EndpointProbeResult::ProtocolMismatch { peer, ours } => Some(format!(
                "{} -> protocol mismatch: peer={peer}, ours={ours}",
                probe.endpoint
            )),
            EndpointProbeResult::Unreachable(reason) => {
                Some(format!("{} -> {reason}", probe.endpoint))
            }
            EndpointProbeResult::Session(_) | EndpointProbeResult::Absent(_) => None,
        })
        .collect()
}

fn probe_report(
    summary: String,
    config_path: Option<&Path>,
    dir_statuses: &[RuntimeDirStatus],
    socket_probes: Vec<SocketProbeDetail>,
) -> ProbeReport {
    ProbeReport {
        summary,
        config_path: config_path.map(|path| path.display().to_string()),
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

/// Resolve a config target: use a file directly or search upward from a directory.
#[must_use]
pub async fn config_for_target(path: &Path) -> Option<PathBuf> {
    if tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
    {
        return tokio::fs::canonicalize(path).await.ok();
    }
    find_config_upward(path).await
}

/// Walk upward from `start` (stopping after the home directory), returning the first
/// canonicalized micromux config path found.
///
/// Hand-rolled instead of delegating to `micromux::find_config_file`: that function's future
/// captures a path-joining closure, which the rmcp `#[tool]` macro's higher-ranked `'static`
/// bound rejects in the MCP tool futures that await this resolver — this future must stay
/// closure-free.
async fn find_config_upward(start: &Path) -> Option<PathBuf> {
    let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    find_config_upward_with_home(start, home.as_deref()).await
}

async fn find_config_upward_with_home(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let mut directory = start.to_path_buf();
    loop {
        for name in micromux::config_file_names() {
            let candidate = directory.join(name);
            if let Ok(canonical) = tokio::fs::canonicalize(&candidate).await {
                return Some(canonical);
            }
        }
        if home.is_some_and(|home| directory == home) {
            return None;
        }
        match directory.parent() {
            Some(parent) => directory = parent.to_path_buf(),
            None => return None,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;

    use micromux::CancellationToken;
    use similar_asserts::assert_eq;

    use super::*;
    use crate::{ControlServer, SessionIdentity, bind};

    struct Running {
        shutdown: CancellationToken,
        _runner: tokio::task::JoinHandle<color_eyre::eyre::Result<()>>,
    }

    fn boot(
        runtime_dir: &Path,
        working_dir: &Path,
        config_path: &Path,
    ) -> color_eyre::eyre::Result<Running> {
        let yaml = "version: 1\nservices:\n  svc:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n";
        let mut diagnostics = Vec::new();
        let config = micromux::from_str(yaml, working_dir, 0usize, None, &mut diagnostics)
            .map_err(|error| color_eyre::eyre::eyre!("parse: {error}"))?;
        let mux = Arc::new(micromux::Micromux::new(&config)?);
        let shutdown = CancellationToken::new();
        let (runner, handles) = mux.clone().start(shutdown.clone());
        let runner = tokio::spawn(runner);
        let endpoint = endpoint_for(runtime_dir, config_path);
        let guard = bind(&endpoint)?.ok_or_else(|| color_eyre::eyre::eyre!("bind failed"))?;
        let identity = SessionIdentity::new("selected".to_string(), working_dir, config_path);
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
    async fn upward_config_search_stops_after_checking_home() -> color_eyre::eyre::Result<()> {
        let root = tempfile::Builder::new()
            .prefix("micromux-control-home-boundary-")
            .tempdir()?;
        let home = root.path().join("home/me");
        let project = home.join("repo/subdir");
        std::fs::create_dir_all(&project)?;
        std::fs::write(
            root.path().join("micromux.yaml"),
            "version: 1\nservices: {}\n",
        )?;

        let found = find_config_upward_with_home(&project, Some(&home)).await;

        assert_eq!(found, None);
        Ok(())
    }

    #[tokio::test]
    async fn upward_config_search_outside_home_still_walks_to_root() -> color_eyre::eyre::Result<()>
    {
        let root = tempfile::Builder::new()
            .prefix("micromux-control-outside-home-")
            .tempdir()?;
        let project = root.path().join("tmp/project");
        std::fs::create_dir_all(&project)?;
        let config = root.path().join("micromux.yaml");
        std::fs::write(&config, "version: 1\nservices: {}\n")?;

        let found = find_config_upward_with_home(&project, Some(Path::new("/home/me"))).await;

        assert_eq!(found, Some(std::fs::canonicalize(config)?));
        Ok(())
    }

    #[test]
    fn selector_parser_preserves_infallible_grammar() {
        assert_eq!(SessionSelector::parse(""), SessionSelector::Current);
        assert_eq!(SessionSelector::parse("current"), SessionSelector::Current);
        assert_eq!(
            SessionSelector::parse("name:api"),
            SessionSelector::Name("api".to_string())
        );
        assert_eq!(SessionSelector::parse("pid:42"), SessionSelector::Pid(42));
        assert_eq!(
            SessionSelector::parse("pid:x"),
            SessionSelector::Name("pid:x".to_string())
        );
        assert_eq!(
            SessionSelector::parse("hash:abc"),
            SessionSelector::ConfigHash("abc".to_string())
        );
    }

    #[tokio::test]
    async fn config_override_selects_a_session_outside_the_cwd() -> color_eyre::eyre::Result<()> {
        let runtime_dir = tempfile::tempdir()?;
        let cwd = tempfile::tempdir()?;
        let project = tempfile::tempdir()?;
        let config_path = project.path().join("micromux.yaml");
        std::fs::write(
            &config_path,
            "version: 1\nservices:\n  svc:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n",
        )?;
        let config_path = std::fs::canonicalize(config_path)?;
        let running = boot(runtime_dir.path(), project.path(), &config_path)?;
        let statuses = vec![RuntimeDirStatus {
            path: runtime_dir.path().to_path_buf(),
            usable: true,
            error: None,
        }];

        let resolved = resolve_selector_in_runtime_dirs(
            &[runtime_dir.path().to_path_buf()],
            &statuses,
            cwd.path(),
            None,
            Some(&config_path),
        )
        .await?;

        assert_eq!(resolved.info.name, "selected");
        assert_eq!(Path::new(&resolved.info.config_path), config_path);
        running.shutdown.cancel();
        Ok(())
    }
}
