//! Read-only, connect-to-verify session selection.
//!
//! The proxy never mutates the filesystem; it only connects. A typed selector resolves to exactly
//! one live session, or a typed error (`NoSession`/`Ambiguous`/`Unsupported`).

use std::path::{Path, PathBuf};

use micromux_control::{
    Client, ControlEndpoint, ControlError, SessionInfo, discover_sessions, endpoint_for,
    endpoint_from_hash, runtime_dir,
};

/// A typed tool error surfaced to the agent.
#[derive(Debug)]
pub enum ToolError {
    /// No live session matched.
    NoSession(String),
    /// A name matched more than one live session.
    Ambiguous(String),
    /// A session endpoint exists but did not answer promptly.
    Busy(String),
    /// The control plane is not supported on this platform.
    Unsupported,
    /// The command is invalid in the service's current state.
    InvalidState(String),
    /// An error reported by the session.
    Remote {
        /// The protocol error code.
        code: micromux_control::ErrorCode,
        /// A human-readable message.
        message: String,
    },
    /// A transport error.
    Control(ControlError),
    /// A response that did not match the request.
    Unexpected(String),
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

/// Walk upward from `start`, returning the first canonicalized micromux config path found.
///
/// Reimplemented here (rather than calling `micromux::find_config_file`) so the future does not
/// capture that function's `|name| dir.join(name)` closure, which the rmcp `#[tool]` macro's
/// higher-ranked `'static` bound on the tool future rejects.
async fn find_config_upward(start: &Path) -> Option<PathBuf> {
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

async fn verify(endpoint: ControlEndpoint) -> Result<Resolved, ToolError> {
    let mut client = Client::connect(&endpoint).await?;
    let info = client.describe().await?;
    Ok(Resolved { endpoint, info })
}

/// Resolve a selector to a single live session.
///
/// # Errors
///
/// Returns [`ToolError::NoSession`] when nothing answers, [`ToolError::Ambiguous`] when a name
/// matches more than one, or [`ToolError::Unsupported`] on a platform without a transport.
pub async fn resolve(cwd: &Path, selector: Option<String>) -> Result<Resolved, ToolError> {
    let runtime_dir =
        runtime_dir().ok_or_else(|| ToolError::NoSession("no runtime directory".to_string()))?;
    resolve_in(&runtime_dir, cwd, selector).await
}

/// Resolve a selector against an explicit runtime directory (testable variant of [`resolve`]).
///
/// # Errors
///
/// See [`resolve`].
pub(crate) async fn resolve_in(
    runtime_dir: &Path,
    cwd: &Path,
    selector: Option<String>,
) -> Result<Resolved, ToolError> {
    match parse_selector(selector) {
        Selector::Current => {
            let config_path = find_config_upward(cwd).await.ok_or_else(|| {
                ToolError::NoSession(
                    "no micromux config found from the current directory".to_string(),
                )
            })?;
            let endpoint = endpoint_for(runtime_dir, &config_path);
            verify(endpoint).await.map_err(|err| {
                map_connect_error(err, || {
                    "no micromux session for this project; start micromux here".to_string()
                })
            })
        }
        Selector::Hash(hash) => {
            let endpoint = endpoint_from_hash(runtime_dir, &hash);
            verify(endpoint)
                .await
                .map_err(|err| map_connect_error(err, || format!("no session with hash {hash}")))
        }
        Selector::Name(name) => {
            let discovery = discover_sessions(runtime_dir)
                .await
                .map_err(ToolError::from)?;
            let unreachable = discovery.unreachable;
            let matches: Vec<_> = discovery
                .sessions
                .into_iter()
                .filter(|session| session.info.name == name)
                .collect();
            pick_unique(matches, unreachable, || format!("name `{name}`"))
        }
        Selector::Pid(pid) => {
            let discovery = discover_sessions(runtime_dir)
                .await
                .map_err(ToolError::from)?;
            let unreachable = discovery.unreachable;
            let matches: Vec<_> = discovery
                .sessions
                .into_iter()
                .filter(|session| session.info.pid == pid)
                .collect();
            pick_unique(matches, unreachable, || format!("pid {pid}"))
        }
    }
}

// `verify()` propagates connect/describe errors through `?`, which applies `From<ControlError>`:
// `Timeout` is already mapped to `Busy` there, so only the hard connection errors need special
// handling here; everything else (including `Busy`) passes through unchanged.
fn map_connect_error(err: ToolError, absent: impl FnOnce() -> String) -> ToolError {
    match err {
        ToolError::Control(ControlError::Io(err))
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            ToolError::NoSession(absent())
        }
        other => other,
    }
}

fn pick_unique(
    mut matches: Vec<micromux_control::DiscoveredSession>,
    unreachable: usize,
    describe: impl Fn() -> String,
) -> Result<Resolved, ToolError> {
    match matches.len() {
        // No answering session matched, but a live endpoint we couldn't describe might be it.
        0 if unreachable > 0 => Err(ToolError::Busy(format!(
            "no answering session matched {}; {unreachable} live session(s) were unreachable and may be the target — retry",
            describe()
        ))),
        0 => Err(ToolError::NoSession(format!(
            "no session matched {}",
            describe()
        ))),
        1 => {
            let session = matches.remove(0);
            Ok(Resolved {
                endpoint: session.endpoint,
                info: session.info,
            })
        }
        _ => Err(ToolError::Ambiguous(format!(
            "{} matched {} live sessions; use pid: or hash: to disambiguate",
            describe(),
            matches.len()
        ))),
    }
}

/// List every session that answers `Describe`. Endpoints that refuse a connection are skipped;
/// live-but-unreachable ones are simply not listed this scan (never pruned).
///
/// # Errors
///
/// Returns [`ToolError::Unsupported`] on a platform without a control transport.
pub async fn list_sessions() -> Result<Vec<SessionInfo>, ToolError> {
    let runtime_dir =
        runtime_dir().ok_or_else(|| ToolError::NoSession("no runtime directory".to_string()))?;
    Ok(discover_sessions(&runtime_dir)
        .await?
        .sessions
        .into_iter()
        .map(|session| session.info)
        .collect())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use micromux::CancellationToken;
    use micromux_control::{ControlServer, SessionIdentity, bind, endpoint_for};
    use similar_asserts::assert_eq;

    struct Running {
        shutdown: CancellationToken,
        _runner: tokio::task::JoinHandle<color_eyre::eyre::Result<()>>,
    }

    fn temp_dir(prefix: &str) -> color_eyre::eyre::Result<PathBuf> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let dir = std::env::temp_dir().join(format!("micromux-mcp-{prefix}-{nanos}"));
        std::fs::create_dir_all(&dir)?;
        Ok(dir)
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
        let config_a = runtime_dir.join("proj-a/micromux.yaml");
        let alpha = boot(&runtime_dir, "alpha", &config_a)?;

        // A leaked, non-connectable socket file must be skipped, never resolved or pruned.
        std::fs::write(runtime_dir.join("dead.sock"), b"")?;

        let resolved = resolve_in(&runtime_dir, Path::new("."), Some("alpha".to_string()))
            .await
            .map_err(|err| color_eyre::eyre::eyre!("{err:?}"))?;
        assert_eq!(resolved.info.name, "alpha");

        let missing = resolve_in(&runtime_dir, Path::new("."), Some("nope".to_string())).await;
        assert!(matches!(missing, Err(ToolError::NoSession(_))));

        // A second session sharing the name must be reported Ambiguous, never picked arbitrarily.
        let config_b = runtime_dir.join("proj-b/micromux.yaml");
        let beta = boot(&runtime_dir, "alpha", &config_b)?;
        let ambiguous = resolve_in(&runtime_dir, Path::new("."), Some("alpha".to_string())).await;
        assert!(matches!(ambiguous, Err(ToolError::Ambiguous(_))));

        alpha.shutdown.cancel();
        beta.shutdown.cancel();
        Ok(())
    }
}
