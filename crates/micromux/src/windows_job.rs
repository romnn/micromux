#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("failed to create a kill-on-close job object")]
    Create(#[source] win32job::JobError),
    #[error("failed to assign the process to its job object")]
    Assign(#[source] win32job::JobError),
}

/// Attach a process to a kill-on-close job so dropping the job terminates remaining descendants.
///
/// Assignment contains descendants created afterward. `portable-pty` exposes the process only
/// after `CreateProcessW` has resumed it, so a descendant created before this call can escape the
/// job; closing that window requires a suspended-spawn API from the PTY backend.
///
/// # Errors
///
/// Returns an error when the job cannot be created or the process cannot be assigned to it.
pub(crate) fn attach_kill_on_close(process: isize) -> Result<win32job::Job, Error> {
    let mut limits = win32job::ExtendedLimitInfo::new();
    limits.limit_kill_on_job_close();
    let job = win32job::Job::create_with_limit_info(&limits).map_err(Error::Create)?;
    job.assign_process(process).map_err(Error::Assign)?;
    Ok(job)
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use color_eyre::eyre;

    const PARENT_HELPER: &str = "windows_job::tests::job_tree_parent_helper";
    const DESCENDANT_HELPER: &str = "windows_job::tests::job_tree_descendant_helper";
    const GATE_ENV: &str = "MICROMUX_WINDOWS_JOB_TEST_GATE";
    const HELD_FILE_ENV: &str = "MICROMUX_WINDOWS_JOB_TEST_HELD_FILE";
    const READY_FILE_ENV: &str = "MICROMUX_WINDOWS_JOB_TEST_READY_FILE";
    const HELPER_TIMEOUT: Duration = Duration::from_secs(30);

    fn required_path(name: &str) -> eyre::Result<PathBuf> {
        std::env::var_os(name)
            .map(PathBuf::from)
            .ok_or_else(|| eyre::eyre!("missing {name}"))
    }

    fn helper_command(test: &str) -> eyre::Result<Command> {
        let mut command = Command::new(std::env::current_exe()?);
        command.args([
            "--ignored",
            "--exact",
            test,
            "--nocapture",
            "--test-threads=1",
        ]);
        Ok(command)
    }

    #[test]
    #[ignore = "spawned by dropping_job_terminates_an_attached_process_tree"]
    fn job_tree_parent_helper() -> eyre::Result<()> {
        let gate = required_path(GATE_ENV)?;
        let deadline = Instant::now() + HELPER_TIMEOUT;
        while !gate.exists() {
            if Instant::now() >= deadline {
                eyre::bail!("timed out waiting for the job-assignment gate");
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let mut descendant = helper_command(DESCENDANT_HELPER)?
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let _status = descendant.wait()?;
        Ok(())
    }

    #[test]
    #[ignore = "spawned by job_tree_parent_helper"]
    fn job_tree_descendant_helper() -> eyre::Result<()> {
        let held_file = required_path(HELD_FILE_ENV)?;
        let ready_file = required_path(READY_FILE_ENV)?;
        let _held = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .share_mode(0)
            .open(held_file)?;
        std::fs::write(ready_file, b"ready")?;
        std::thread::sleep(Duration::from_mins(5));
        Ok(())
    }

    #[test]
    fn dropping_job_terminates_an_attached_process_tree() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let gate = directory.path().join("start");
        let held_file = directory.path().join("held");
        let ready_file = directory.path().join("ready");
        let mut parent = helper_command(PARENT_HELPER)?
            .env(GATE_ENV, &gate)
            .env(HELD_FILE_ENV, &held_file)
            .env(READY_FILE_ENV, &ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let job = match super::attach_kill_on_close(parent.as_raw_handle() as isize) {
            Ok(job) => job,
            Err(err) => {
                let _ = parent.kill();
                let _ = parent.wait();
                return Err(err.into());
            }
        };
        // Hold the parent behind the gate until assignment so its descendant must inherit the job.
        std::fs::write(&gate, b"go")?;
        let deadline = Instant::now() + HELPER_TIMEOUT;
        while !ready_file.exists() {
            if let Some(status) = parent.try_wait()? {
                eyre::bail!("the attached parent exited before its descendant was ready: {status}");
            }
            if Instant::now() >= deadline {
                eyre::bail!("the attached parent did not create its descendant");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // The exclusive handle follows this exact descendant; a numeric PID can be recycled while
        // teardown is still being polled.
        if std::fs::remove_file(&held_file).is_ok() {
            eyre::bail!("the descendant did not retain its exclusive file handle");
        }

        drop(job);

        let deadline = Instant::now() + HELPER_TIMEOUT;
        let mut descendant_exited = false;
        while parent.try_wait()?.is_none() || !descendant_exited {
            if !descendant_exited {
                descendant_exited = std::fs::remove_file(&held_file).is_ok();
            }
            if Instant::now() >= deadline {
                eyre::bail!("dropping the job did not terminate the full process tree");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}
