//! Deterministic endpoint naming and runtime-dir resolution.
//!
//! The endpoint name is a fixed-length digest of the *canonical config path*, so the session and
//! the proxy derive the identical name from the same input and the common one-session-per-project
//! case needs no enumeration.

use std::borrow::Cow;
use std::fmt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// A control endpoint, keyed by filesystem path / pipe name. The supported transports are a closed
/// set: no network transport, ever.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ControlEndpoint {
    /// A Unix domain socket (Unix/macOS).
    Unix(PathBuf),
    /// A Windows named pipe (M1-Windows; currently unsupported).
    WindowsNamedPipe(String),
}

impl fmt::Display for ControlEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "{}", path.display()),
            Self::WindowsNamedPipe(name) => f.write_str(name),
        }
    }
}

/// Preparation status for one plausible runtime directory.
#[derive(Debug, Clone)]
pub struct RuntimeDirStatus {
    /// Runtime directory path.
    pub path: PathBuf,
    /// Whether the directory was usable.
    pub usable: bool,
    /// Preparation error for unusable directories.
    pub error: Option<String>,
}

/// Extract the usable paths from runtime-dir preparation results.
#[must_use]
pub fn usable_runtime_dirs(statuses: &[RuntimeDirStatus]) -> Vec<PathBuf> {
    statuses
        .iter()
        .filter(|status| status.usable)
        .map(|status| status.path.clone())
        .collect()
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
    let input = endpoint_hash_input(config_path);
    let digest = Sha256::digest(input.as_ref());
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(unix)]
fn endpoint_hash_input(config_path: &Path) -> Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;

    Cow::Borrowed(config_path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn endpoint_hash_input(config_path: &Path) -> Cow<'_, [u8]> {
    Cow::Owned(config_path.to_string_lossy().into_owned().into_bytes())
}

/// Resolve the per-user runtime directory for micromux endpoints, creating it with restrictive
/// permissions. Returns `None` when no directory can be resolved (control plane disabled).
#[must_use]
pub fn runtime_dir() -> Option<PathBuf> {
    for path in runtime_dir_candidates() {
        match ensure_private_dir(&path) {
            Ok(()) => return Some(path),
            Err(err) => {
                tracing::warn!(?err, dir = %path.display(), "failed to prepare runtime dir");
            }
        }
    }
    None
}

/// Report every plausible runtime directory and whether it is currently usable.
#[must_use]
pub fn runtime_dir_statuses() -> Vec<RuntimeDirStatus> {
    runtime_dir_candidates()
        .into_iter()
        .map(|path| match ensure_private_dir(&path) {
            Ok(()) => RuntimeDirStatus {
                path,
                usable: true,
                error: None,
            },
            Err(err) => RuntimeDirStatus {
                path,
                usable: false,
                error: Some(err.to_string()),
            },
        })
        .collect()
}

fn runtime_dir_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(directory) = micromux::project_dir()
        .and_then(|directories| directories.runtime_dir().map(Path::to_path_buf))
    {
        push_unique(&mut candidates, directory);
    }

    #[cfg(unix)]
    if let Some(directory) = xdg_runtime_directory() {
        push_unique(&mut candidates, directory);
    }

    #[cfg(all(unix, target_os = "linux"))]
    if let Some(directory) =
        systemd_runtime_directory(Path::new("/run/user"), nix::unistd::Uid::current())
    {
        push_unique(&mut candidates, directory);
    }

    push_unique(&mut candidates, per_user_fallback_dir());
    candidates
}

fn push_unique(candidates: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
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
fn xdg_runtime_directory() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").and_then(|directory| {
        if directory.is_empty() {
            None
        } else {
            Some(PathBuf::from(directory).join("micromux"))
        }
    })
}

#[cfg(all(unix, target_os = "linux"))]
fn systemd_runtime_directory(base: &Path, user_identifier: nix::unistd::Uid) -> Option<PathBuf> {
    let user_runtime_directory = base.join(user_identifier.as_raw().to_string());
    user_runtime_directory
        .is_dir()
        .then(|| user_runtime_directory.join("micromux"))
}

#[cfg(unix)]
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    use nix::fcntl::{OFlag, open};
    use nix::sys::stat::{Mode, SFlag, fchmod, fstat};
    use std::os::unix::fs::DirBuilderExt;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;

    let fd = open(
        dir,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(nix_to_io)?;
    let metadata = fstat(&fd).map_err(nix_to_io)?;
    if !SFlag::from_bits_truncate(metadata.st_mode).contains(SFlag::S_IFDIR) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "runtime dir path is not a directory",
        ));
    }
    if metadata.st_uid != nix::unistd::Uid::current().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "runtime dir is not owned by the current user",
        ));
    }

    // `recursive` does not re-apply the mode to a pre-existing dir; the directory mode is what gates
    // `connect`, so set it defensively.
    if metadata.st_mode & 0o777 != 0o700 {
        fchmod(&fd, Mode::from_bits_truncate(0o700)).map_err(nix_to_io)?;
    }
    Ok(())
}

#[cfg(unix)]
fn nix_to_io(err: nix::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(err as i32)
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
    #[cfg(all(unix, target_os = "linux"))]
    use std::io;

    #[test]
    fn endpoint_hash_is_deterministic_and_fixed_length() {
        let a = endpoint_hash(Path::new("/home/user/project/micromux.yaml"));
        let b = endpoint_hash(Path::new("/home/user/project/micromux.yaml"));
        let c = endpoint_hash(Path::new("/home/user/other/micromux.yaml"));
        assert_eq!(a, b);
        assert_eq!(a, "3f98f83e6e2c8d23");
        assert_eq!(a.len(), 16);
        assert_ne!(a, c);
    }

    #[cfg(unix)]
    #[test]
    fn endpoint_hash_uses_raw_unix_path_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let first = PathBuf::from(OsString::from_vec(b"/tmp/micromux-\xff.yaml".to_vec()));
        let second = PathBuf::from(OsString::from_vec(b"/tmp/micromux-\xfe.yaml".to_vec()));

        assert_ne!(endpoint_hash(&first), endpoint_hash(&second));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_rejects_symlink_leaf() -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::Builder::new()
            .prefix("micromux-control-symlink-test-")
            .tempdir()?;

        let target = root.path().join("target");
        std::fs::create_dir_all(&target)?;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link)?;

        let err = ensure_private_dir(&link).expect_err("symlink leaf must be rejected");
        assert!(is_symlink_leaf_error(&err));
        assert_eq!(
            std::fs::metadata(&target)?.permissions().mode() & 0o777,
            0o755,
            "the symlink target must not be chmodded"
        );
        Ok(())
    }

    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn inferred_systemd_runtime_directory_matches_project_runtime_shape() -> io::Result<()> {
        let root = tempfile::Builder::new()
            .prefix("micromux-control-endpoint-test-")
            .tempdir()?;

        let user_identifier = nix::unistd::Uid::from_raw(4242);
        assert_eq!(
            systemd_runtime_directory(root.path(), user_identifier),
            None
        );

        let user_runtime_directory = root.path().join("4242");
        std::fs::create_dir_all(&user_runtime_directory)?;
        assert_eq!(
            systemd_runtime_directory(root.path(), user_identifier),
            Some(user_runtime_directory.join("micromux"))
        );

        Ok(())
    }

    #[cfg(unix)]
    fn is_symlink_leaf_error(err: &std::io::Error) -> bool {
        matches!(
            err.kind(),
            std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotADirectory
        ) || err.raw_os_error() == Some(nix::errno::Errno::ELOOP as i32)
    }
}
