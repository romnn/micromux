//! The per-session control server: bind (race-safe), accept, dispatch.
//!
//! The server holds two model capabilities — a [`SessionModelReader`] (queries + `subscribe`) and a
//! [`ServiceControl`] (mutations) — plus the session's shutdown token (passed to `serve`), which a
//! `Request::Shutdown` cancels to stop the whole session. It has no model writer, so a request
//! becomes a write only after the scheduler processes it, and no input port, so `SendInput`/
//! `ResizeAll` are not expressible.

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
use crate::endpoint::{ControlEndpoint, endpoint_hash};

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
    pub fn new(name: String, working_dir: &Path, config_path: &Path) -> Self {
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
}

impl ControlServer {
    /// Construct a control server over a read capability and a narrow command port.
    #[must_use]
    pub fn new(
        reader: SessionModelReader,
        control: ServiceControl,
        identity: SessionIdentity,
    ) -> Self {
        Self {
            reader,
            control,
            identity,
        }
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

        let config_path = PathBuf::from(OsString::from_vec(b"/tmp/micromux-\xff.yaml".to_vec()));
        let identity = SessionIdentity::new("test".to_string(), Path::new("."), &config_path);

        assert_eq!(identity.id, endpoint_hash(&config_path));
        assert_ne!(identity.id, endpoint_hash(Path::new(&identity.config_path)));
    }
}
