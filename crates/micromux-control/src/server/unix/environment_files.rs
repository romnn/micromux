//! The dotenv files a session writes for `micromux env --file`.
//!
//! They live in a private directory next to the session's control socket.
//! The session recreates the directory when it binds its endpoint and removes it when it unbinds
//! it, both under the endpoint lock, so no file outlives the session that wrote it; a crashed
//! session's files go when the next session for its config binds the endpoint.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The directory for the environment files of the session whose socket is `socket_path`.
pub(super) fn directory(socket_path: &Path) -> PathBuf {
    socket_path.with_extension("env.d")
}

/// Replaces whatever a crashed predecessor left at `directory` with an empty directory only its
/// owner can enter.
///
/// Only the endpoint's owner may call it, under the endpoint lock.
pub(super) fn reset(directory: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    remove(directory)?;
    std::fs::DirBuilder::new().mode(0o700).create(directory)
}

/// Removes `directory` and every file in it.
pub(super) fn remove(directory: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(directory) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

/// Writes `text` to the file for `service`, or for the session's own environment without one,
/// and returns its path.
///
/// The text goes to a new file only its owner can read, which then replaces the file at that
/// path in one step, so a reader sees the previous file or this one, never part of either.
/// The directory is never created here: once the stopping session removed it, a late write fails
/// instead of leaving a file behind.
///
/// `service` must be a valid service id, which is always one path component.
pub(super) fn write(
    directory: &Path,
    service: Option<&str>,
    text: &str,
) -> std::io::Result<PathBuf> {
    let path = directory.join(match service {
        Some(service) => format!("service-{service}.env"),
        None => "session.env".to_string(),
    });
    let mut file = tempfile::Builder::new()
        .prefix(".")
        .suffix(".tmp")
        .tempfile_in(directory)?;
    file.write_all(text.as_bytes())?;
    file.persist(&path).map_err(|err| err.error)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use color_eyre::eyre;
    use similar_asserts::assert_eq;

    use super::{directory, remove, reset, write};

    fn mode(path: &std::path::Path) -> eyre::Result<u32> {
        Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
    }

    /// Files are private, rewritten in place, and gone with their directory, which a late write
    /// never brings back.
    #[test]
    fn environment_files_are_private_replaced_and_removed() -> eyre::Result<()> {
        let runtime = tempfile::tempdir()?;
        let directory = directory(&runtime.path().join("0123456789abcdef.sock"));
        assert_eq!(
            directory,
            runtime.path().join("0123456789abcdef.env.d"),
            "the directory sits next to the socket"
        );

        // A crashed predecessor's file is gone after a reset.
        std::fs::create_dir(&directory)?;
        std::fs::write(directory.join("session.env"), "STALE=\"1\"\n")?;
        reset(&directory)?;
        assert_eq!(std::fs::read_dir(&directory)?.count(), 0);
        assert_eq!(mode(&directory)?, 0o700);

        let session = write(&directory, None, "A=\"1\"\n")?;
        assert_eq!(session, directory.join("session.env"));
        assert_eq!(std::fs::read_to_string(&session)?, "A=\"1\"\n");
        assert_eq!(mode(&session)?, 0o600);

        // A rewrite replaces the file at the same path and leaves no temporary file behind.
        assert_eq!(write(&directory, None, "A=\"2\"\n")?, session);
        assert_eq!(std::fs::read_to_string(&session)?, "A=\"2\"\n");
        let service = write(&directory, Some("api"), "B=\"3\"\n")?;
        assert_eq!(service, directory.join("service-api.env"));
        let mut names = std::fs::read_dir(&directory)?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<eyre::Result<Vec<_>>>()?;
        names.sort();
        assert_eq!(names, ["service-api.env", "session.env"]);

        remove(&directory)?;
        assert!(!directory.exists());
        assert!(write(&directory, None, "A=\"4\"\n").is_err());
        assert!(!directory.exists());
        // Removing twice is not an error.
        remove(&directory)?;
        Ok(())
    }
}
