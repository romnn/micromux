//! Where the proxy's singleton lock and registration socket live.

use std::path::PathBuf;

/// The files every session and the proxy must agree on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyPaths {
    /// Unix socket sessions register over.
    pub socket: PathBuf,
    /// Lock held by the one running proxy for its whole lifetime.
    pub lock: PathBuf,
}

impl ProxyPaths {
    /// The current user's paths, in micromux's fixed per-user coordination directory.
    ///
    /// `MICROMUX_STATE_DIR` relocates them along with the rest of micromux's state (see
    /// [`micromux_control::coordination_dir`]).
    ///
    /// # Errors
    ///
    /// Returns an error when the coordination directory cannot be prepared.
    pub fn user() -> std::io::Result<Self> {
        Ok(Self::in_dir(micromux_control::coordination_dir()?))
    }

    /// Paths inside `directory`, for tests and isolated setups.
    #[must_use]
    pub fn in_dir(directory: impl Into<PathBuf>) -> Self {
        let directory = directory.into();
        Self {
            socket: directory.join("proxy.sock"),
            lock: directory.join("proxy.lock"),
        }
    }
}
