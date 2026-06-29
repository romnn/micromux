//! Deterministic endpoint naming and runtime-dir resolution.
//!
//! The endpoint name is a fixed-length digest of the *canonical config path*, so the session and
//! the proxy derive the identical name from the same input and the common one-session-per-project
//! case needs no enumeration.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// A control endpoint, keyed by filesystem path / pipe name. The supported transports are a closed
/// set: no network transport, ever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlEndpoint {
    /// A Unix domain socket (Unix/macOS).
    Unix(PathBuf),
    /// A Windows named pipe (M1-Windows; currently unsupported).
    WindowsNamedPipe(String),
}

/// Whether this build has a concrete control transport implementation.
#[cfg(unix)]
#[must_use]
pub const fn transport_supported() -> bool {
    true
}

/// Whether this build has a concrete control transport implementation.
#[cfg(not(unix))]
#[must_use]
pub const fn transport_supported() -> bool {
    false
}

/// The deterministic short digest of a canonical config path. Fixed length keeps the macOS
/// `sun_path` budget under control.
#[must_use]
pub fn endpoint_hash(config_path: &Path) -> String {
    fn hex_nibble(nibble: u8) -> char {
        match nibble {
            0..=9 => char::from(b'0' + nibble),
            10..=15 => char::from(b'a' + (nibble - 10)),
            _ => '?',
        }
    }

    let digest = Sha256::digest(config_path.to_string_lossy().as_bytes());
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        out.push(hex_nibble(byte >> 4));
        out.push(hex_nibble(byte & 0x0f));
    }
    out
}

/// Resolve the per-user runtime directory for micromux endpoints, creating it with restrictive
/// permissions. Returns `None` when no directory can be resolved (control plane disabled).
#[must_use]
pub fn runtime_dir() -> Option<PathBuf> {
    let base = micromux::project_dir()
        .and_then(|dirs| dirs.runtime_dir().map(Path::to_path_buf))
        .unwrap_or_else(per_user_fallback_dir);
    if let Err(err) = ensure_private_dir(&base) {
        tracing::warn!(?err, dir = %base.display(), "failed to prepare runtime dir");
        return None;
    }
    Some(base)
}

#[cfg(unix)]
fn per_user_fallback_dir() -> PathBuf {
    let uid = nix::unistd::Uid::current();
    std::env::temp_dir().join(format!("micromux-{uid}"))
}

#[cfg(not(unix))]
fn per_user_fallback_dir() -> PathBuf {
    std::env::temp_dir().join("micromux")
}

#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // `recursive` does not re-apply the mode to a pre-existing dir; the directory mode is what gates
    // `connect`, so set it defensively.
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

/// Build a control endpoint from a known endpoint hash within `runtime_dir`.
#[must_use]
pub fn endpoint_from_hash(runtime_dir: &Path, hash: &str) -> ControlEndpoint {
    #[cfg(unix)]
    {
        ControlEndpoint::Unix(runtime_dir.join(format!("{hash}.sock")))
    }
    #[cfg(not(unix))]
    {
        let _ = runtime_dir;
        ControlEndpoint::WindowsNamedPipe(format!(r"\\.\pipe\micromux-{hash}"))
    }
}

/// Compute the control endpoint for a session's canonical config path within `runtime_dir`.
#[must_use]
pub fn endpoint_for(runtime_dir: &Path, config_path: &Path) -> ControlEndpoint {
    endpoint_from_hash(runtime_dir, &endpoint_hash(config_path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use similar_asserts::assert_eq;

    #[test]
    fn endpoint_hash_is_deterministic_and_fixed_length() {
        let a = endpoint_hash(Path::new("/home/user/project/micromux.yaml"));
        let b = endpoint_hash(Path::new("/home/user/project/micromux.yaml"));
        let c = endpoint_hash(Path::new("/home/user/other/micromux.yaml"));
        assert_eq!(a, b);
        assert_eq!(a, "3f98f83e6e2c8d23");
        assert_eq!(a.len(), 16);
        assert!(a != c);
    }
}
