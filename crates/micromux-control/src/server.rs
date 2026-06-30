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
use std::time::{SystemTime, UNIX_EPOCH};

use micromux::{ServiceControl, SessionModelReader};
use tokio_util::sync::CancellationToken;

use crate::ControlError;
use crate::endpoint::ControlEndpoint;

pub use platform::EndpointGuard;

/// Static session identity used to answer `Describe`.
#[derive(Debug, Clone)]
pub struct SessionIdentity {
    /// Process id of the session.
    pub pid: u32,
    /// Session start time as a Unix timestamp (seconds).
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
        let start_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            pid: std::process::id(),
            start_time,
            name,
            working_dir: working_dir.to_string_lossy().into_owned(),
            config_path: config_path.to_string_lossy().into_owned(),
            micromux_version: env!("CARGO_PKG_VERSION").to_string(),
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

/// The control server. Cheap to clone-share via `Arc`.
#[cfg_attr(not(unix), allow(dead_code))]
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
