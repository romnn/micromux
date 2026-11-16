//! The dotenv files a session writes for `micromux env --file`.
//!
//! They live in a private directory next to the session's control socket.
//! The session recreates the directory when it binds its endpoint and removes it when it unbinds
//! it, both under the endpoint lock, so no file outlives the session that wrote it; a crashed
//! session's files go when the next session for its config binds the endpoint.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock};

/// The private directory a session writes its environment files to, and whether it still may.
///
/// A write holds the lock shared from its first file operation to its last, and [`Self::close`]
/// takes it exclusively, so closing waits for every write that started and refuses every later
/// one.
/// Without that, a write still running after its request was abandoned could create a file
/// after the removal listed the directory, and that file, which may hold secrets, would stay.
#[derive(Debug)]
pub(super) struct EnvironmentFiles {
    directory: PathBuf,
    /// Whether writes are still accepted.
    open: RwLock<bool>,
}

impl EnvironmentFiles {
    /// Replaces whatever a crashed predecessor left next to the socket at `socket_path` with an
    /// empty directory only its owner can enter.
    ///
    /// Only the endpoint's owner may call it, under the endpoint lock.
    pub(super) fn create(socket_path: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt as _;

        let directory = socket_path.with_extension("env.d");
        remove(&directory)?;
        std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
        Ok(Self {
            directory,
            open: RwLock::new(true),
        })
    }

    /// Writes `text` to the file for `service`, or for the session's own environment without
    /// one, and returns its path.
    ///
    /// The text goes to a new file only its owner can read, which then replaces the file at that
    /// path in one step, so a reader sees the previous file or this one, never part of either.
    ///
    /// `service` must be a valid service id, which is always one path component.
    ///
    /// # Errors
    ///
    /// Returns an error when the files are closed, or the file cannot be written.
    pub(super) fn write(&self, service: Option<&str>, text: &str) -> std::io::Result<PathBuf> {
        let open = self.open.read().unwrap_or_else(PoisonError::into_inner);
        if !*open {
            return Err(std::io::Error::other("the session is stopping"));
        }
        let path = self.directory.join(match service {
            Some(service) => format!("service-{service}.env"),
            None => "session.env".to_string(),
        });
        let mut file = tempfile::Builder::new()
            .prefix(".")
            .suffix(".tmp")
            .tempfile_in(&self.directory)?;
        file.write_all(text.as_bytes())?;
        file.persist(&path).map_err(|err| err.error)?;
        Ok(path)
    }

    /// Refuses every further write, waits for the running ones, and removes the directory and
    /// every file in it.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory cannot be removed.
    pub(super) fn close(&self) -> std::io::Result<()> {
        let mut open = self.open.write().unwrap_or_else(PoisonError::into_inner);
        *open = false;
        remove(&self.directory)
    }

    /// The directory the files live in.
    pub(super) fn directory(&self) -> &Path {
        &self.directory
    }
}

/// Removes `directory` and every file in it.
fn remove(directory: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(directory) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::PoisonError;
    use std::time::Duration;

    use color_eyre::eyre;
    use similar_asserts::assert_eq;

    use super::EnvironmentFiles;

    fn mode(path: &std::path::Path) -> eyre::Result<u32> {
        Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
    }

    /// Files are private, rewritten in place, and gone with their directory, which a late write
    /// never brings back.
    #[test]
    fn environment_files_are_private_replaced_and_removed() -> eyre::Result<()> {
        let runtime = tempfile::tempdir()?;
        let socket = runtime.path().join("0123456789abcdef.sock");
        let directory = runtime.path().join("0123456789abcdef.env.d");

        // A crashed predecessor's file is gone once the endpoint is bound again.
        std::fs::create_dir(&directory)?;
        std::fs::write(directory.join("session.env"), "STALE=\"1\"\n")?;
        let files = EnvironmentFiles::create(&socket)?;
        assert_eq!(
            files.directory(),
            directory,
            "the directory sits next to the socket"
        );
        assert_eq!(std::fs::read_dir(&directory)?.count(), 0);
        assert_eq!(mode(&directory)?, 0o700);

        let session = files.write(None, "A=\"1\"\n")?;
        assert_eq!(session, directory.join("session.env"));
        assert_eq!(std::fs::read_to_string(&session)?, "A=\"1\"\n");
        assert_eq!(mode(&session)?, 0o600);

        // A rewrite replaces the file at the same path and leaves no temporary file behind.
        assert_eq!(files.write(None, "A=\"2\"\n")?, session);
        assert_eq!(std::fs::read_to_string(&session)?, "A=\"2\"\n");
        let service = files.write(Some("api"), "B=\"3\"\n")?;
        assert_eq!(service, directory.join("service-api.env"));
        let mut names = std::fs::read_dir(&directory)?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<eyre::Result<Vec<_>>>()?;
        names.sort();
        assert_eq!(names, ["service-api.env", "session.env"]);

        files.close()?;
        assert!(!directory.exists());
        assert!(files.write(None, "A=\"4\"\n").is_err());
        assert!(!directory.exists());
        // Closing twice is not an error.
        files.close()?;
        Ok(())
    }

    /// Closing waits for a write that is under way, so a file it creates late is removed with
    /// the directory instead of outliving the session.
    #[test]
    fn closing_waits_for_a_running_write() -> eyre::Result<()> {
        let runtime = tempfile::tempdir()?;
        let files = EnvironmentFiles::create(&runtime.path().join("0123456789abcdef.sock"))?;
        let directory = files.directory().to_path_buf();

        std::thread::scope(|scope| -> eyre::Result<()> {
            // Hold the lock a write holds, as a write that is under way does.
            let writing = files.open.read().unwrap_or_else(PoisonError::into_inner);
            let closing = scope.spawn(|| files.close());
            // A close that did not wait would remove the directory now, and the late file below
            // would have nowhere to go.
            std::thread::sleep(Duration::from_millis(100));
            std::fs::write(directory.join(".late.tmp"), "SECRET=\"1\"\n")?;
            drop(writing);
            closing
                .join()
                .map_err(|_| eyre::eyre!("closing panicked"))??;
            Ok(())
        })?;

        assert!(!directory.exists(), "the late file outlived the session");
        Ok(())
    }
}
