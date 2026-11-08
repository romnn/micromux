//! The per-session control server: bind (race-safe), accept, dispatch.
//!
//! The server holds two model capabilities — a [`SessionModelReader`] (queries + `subscribe`) and a
//! [`ServiceControl`] (mutations and the environment read) — plus the session's shutdown token
//! (passed to `serve`), which a `Request::Shutdown` cancels to stop the whole session.
//! It has no model writer, so a request becomes a write only after the scheduler processes it,
//! and no terminal port, so PTY input and resize requests are not expressible.

#[cfg(not(unix))]
#[path = "server/unsupported.rs"]
mod platform;
#[cfg(unix)]
#[path = "server/unix.rs"]
mod platform;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use micromux::{ServiceControl, SessionModelReader};
use tokio_util::sync::CancellationToken;

use crate::ControlError;
use crate::endpoint::{CanonicalConfigPath, ControlEndpoint, endpoint_hash};

pub use platform::EndpointGuard;

static LAST_SESSION_START_TOKEN: AtomicU64 = AtomicU64::new(0);

/// Static session identity used to answer `Describe`.
#[derive(Debug, Clone)]
pub struct SessionIdentity {
    /// Deterministic endpoint hash of the canonical config path.
    pub id: String,
    /// Process id of the session.
    pub pid: u32,
    /// Monotonic session start token as Unix nanoseconds.
    pub start_time: u64,
    /// Session name.
    pub name: String,
    /// The directory the session was launched in.
    pub working_dir: String,
    /// The canonical config path keying this session's endpoint.
    pub config_path: String,
    /// The micromux version of the session binary.
    pub micromux_version: String,
}

impl SessionIdentity {
    /// Build an identity for the current process.
    #[must_use]
    pub fn new(name: String, working_dir: &Path, config_path: &CanonicalConfigPath) -> Self {
        Self {
            id: endpoint_hash(config_path),
            pid: std::process::id(),
            start_time: session_start_token(),
            name,
            working_dir: working_dir.to_string_lossy().into_owned(),
            config_path: config_path.to_string_lossy().into_owned(),
            micromux_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

fn session_start_token() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
        });
    let mut previous = LAST_SESSION_START_TOKEN.load(Ordering::Relaxed);
    loop {
        let next = now.max(previous.saturating_add(1));
        match LAST_SESSION_START_TOKEN.compare_exchange_weak(
            previous,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return next,
            Err(observed) => previous = observed,
        }
    }
}

/// Bind the control endpoint via the race-safe lifetime-lock dance.
///
/// Returns `Ok(Some(guard))` when this process acquired the per-hash ownership lock and bound the
/// endpoint; `Ok(None)` when a live owner already holds this project (second-instance policy: run
/// with control disabled); `Err(Unsupported)` on a platform without a transport.
///
/// # Errors
///
/// Returns [`ControlError::Io`] if the lock file or socket cannot be created/bound, or
/// [`ControlError::Unsupported`] on an unsupported platform.
pub fn bind(endpoint: &ControlEndpoint) -> Result<Option<EndpointGuard>, ControlError> {
    platform::bind(endpoint)
}

/// How the error a session reports when another session holds its config's owner lock begins,
/// for a process that only sees the session's stderr, such as the one that started it.
pub const SESSION_ALREADY_STARTING: &str =
    "a session for this config is already starting or running";

/// A config's owner lock, which makes the config's session unique.
///
/// A session claims it before it waits for anything, so a second start for the same config fails
/// at once, and [`bind_project`] then serves the endpoint under it.
/// Dropping it releases the lock.
#[derive(Debug)]
pub struct ProjectClaim {
    // The OS releases the advisory lock when the last descriptor closes, also on a crash.
    #[cfg_attr(
        not(unix),
        expect(
            dead_code,
            reason = "platforms without a control transport take no lock, so nothing reads it"
        )
    )]
    lock: Option<std::fs::File>,
}

/// Why [`claim_project`] could not claim a config.
#[derive(Debug, thiserror::Error)]
pub enum ClaimError {
    /// Another session holds the config's owner lock.
    #[error(
        "{SESSION_ALREADY_STARTING}{}; stop it first, or attach to it with `micromux attach`",
        pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default()
    )]
    Held {
        /// The process id the holder recorded, when it could be read.
        pid: Option<u32>,
    },
    /// The lock file could not be opened or locked.
    #[error(transparent)]
    Control(#[from] ControlError),
}

/// Claims the owner lock of `config_path` for this process, and records its process id there.
///
/// The lock lives in a fixed per-user directory under `/tmp`, so changing `XDG_RUNTIME_DIR` or
/// `TMPDIR` cannot start another supervisor for the same config.
/// On a platform without a control transport the claim holds no lock.
///
/// # Errors
///
/// Returns [`ClaimError::Held`] when another process holds the lock, and
/// [`ClaimError::Control`] when the lock file cannot be opened or locked.
pub fn claim_project(config_path: &CanonicalConfigPath) -> Result<ProjectClaim, ClaimError> {
    platform::claim_project(config_path)
}

/// Claims the owner lock at `lock_path` like [`claim_project`], for tests that keep their locks
/// out of the user's coordination directory.
///
/// # Errors
///
/// Returns the same errors as [`claim_project`].
#[cfg(unix)]
pub fn claim_project_at(lock_path: &std::path::Path) -> Result<ProjectClaim, ClaimError> {
    platform::claim_project_at(lock_path)
}

/// Binds a session endpoint under the config's owner lock, which `claim` holds.
///
/// The returned guard shares the owner lock, which stays held until both the guard and `claim`
/// are dropped.
///
/// # Errors
///
/// Returns [`ControlError::Io`] if the endpoint lock or the socket cannot be created, or
/// [`ControlError::Unsupported`] when this platform has no control transport.
pub fn bind_project(
    endpoint: &ControlEndpoint,
    claim: &ProjectClaim,
) -> Result<Option<EndpointGuard>, ControlError> {
    platform::bind_project(endpoint, claim)
}

/// Return whether a live owner currently holds the endpoint's lifetime lock.
///
/// This is a read-only diagnostic/probing operation: unlike [`bind`], it never unlinks a socket or
/// creates a missing lock file.
///
/// # Errors
///
/// Returns [`ControlError::Io`] if the existing lock file cannot be opened or probed, or
/// [`ControlError::Unsupported`] on an unsupported platform.
pub fn endpoint_owner_lock_held(endpoint: &ControlEndpoint) -> Result<bool, ControlError> {
    platform::endpoint_owner_lock_held(endpoint)
}

/// The control server. Cheap to clone-share via `Arc`.
#[cfg_attr(
    not(unix),
    expect(
        dead_code,
        reason = "unsupported platforms construct the shared type but do not have a server transport that reads the fields"
    )
)]
pub struct ControlServer {
    reader: SessionModelReader,
    control: ServiceControl,
    identity: SessionIdentity,
    dynamic_policy: micromux::DynamicServicesPolicy,
    session: Option<micromux::SessionStatus>,
}

impl ControlServer {
    /// Construct a control server over a read capability and a narrow command port.
    #[must_use]
    pub fn new(
        reader: SessionModelReader,
        control: ServiceControl,
        identity: SessionIdentity,
        dynamic_policy: micromux::DynamicServicesPolicy,
    ) -> Self {
        Self {
            reader,
            control,
            identity,
            dynamic_policy,
            session: None,
        }
    }

    /// Reports the session's name, URLs, and allocated ports in [`crate::Request::Describe`].
    #[must_use]
    pub fn with_session_status(mut self, session: micromux::SessionStatus) -> Self {
        self.session = Some(session);
        self
    }

    /// Accept connections until `shutdown`, then unlink the endpoint (via the guard's `Drop`).
    ///
    /// # Errors
    ///
    /// Returns [`ControlError::Unsupported`] on a platform without a transport, or an I/O error if
    /// accepting fails fatally.
    pub async fn serve(
        self: Arc<Self>,
        guard: EndpointGuard,
        shutdown: CancellationToken,
    ) -> Result<(), ControlError> {
        platform::serve(self, guard, shutdown).await
    }
}

/// Platforms without a control transport must degrade to a typed `Unsupported` error — never
/// panic, never bind. This is the contract the CLI's `control::spawn` and every client rely on.
#[cfg(all(test, not(unix)))]
mod unsupported_transport_tests {
    use std::path::Path;

    use super::{bind, endpoint_owner_lock_held};
    use crate::ControlError;
    use crate::endpoint::endpoint_from_hash;

    #[test]
    fn bind_and_owner_probe_report_unsupported_transport() {
        let endpoint = endpoint_from_hash(Path::new("ignored"), "0011223344556677");

        assert!(matches!(bind(&endpoint), Err(ControlError::Unsupported)));
        assert!(matches!(
            endpoint_owner_lock_held(&endpoint),
            Err(ControlError::Unsupported)
        ));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn session_start_token_is_unique_for_fast_successive_sessions() {
        let first = session_start_token();
        let second = session_start_token();

        assert_ne!(first, second);
    }

    #[test]
    fn session_identity_id_uses_raw_config_path_hash() {
        use similar_asserts::assert_eq;
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        use std::path::PathBuf;

        // A path whose bytes are not valid UTF-8: the identity must hash the raw bytes, not the
        // lossy string it advertises to clients. Built through the trusted constructor rather than
        // a real file because macOS filesystems reject non-UTF-8 filenames outright (EILSEQ), and
        // the property under test is byte handling, not canonicalization.
        let config_path = CanonicalConfigPath::from_canonicalized(PathBuf::from(
            OsString::from_vec(b"/tmp/micromux-\xff.yaml".to_vec()),
        ));
        let identity = SessionIdentity::new("test".to_string(), Path::new("."), &config_path);

        assert_eq!(identity.id, endpoint_hash(&config_path));
        assert_ne!(
            identity.id,
            crate::endpoint::hash_path(Path::new(&identity.config_path))
        );
    }
}
