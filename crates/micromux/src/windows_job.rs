#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("failed to create a kill-on-close job object")]
    Create(#[source] win32job::JobError),
    #[error("failed to assign the process to its job object")]
    Assign(#[source] win32job::JobError),
}

/// Attach a process to a kill-on-close job so dropping the job terminates remaining descendants.
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
    use std::os::windows::io::AsRawHandle as _;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use color_eyre::eyre;

    fn powershell_literal(path: &std::path::Path) -> String {
        path.to_string_lossy().replace('\'', "''")
    }

    fn process_exists(pid: u32) -> eyre::Result<bool> {
        let output = Command::new("tasklist.exe")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]) // spellcheck:ignore-line
            .output()?;
        let pid = pid.to_string();
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.split(',').nth(1))
            .any(|field| field.trim_matches('"') == pid))
    }

    #[test]
    fn dropping_job_terminates_an_attached_process_tree() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let gate = directory.path().join("start");
        let child_pid = directory.path().join("child.pid");
        let script = format!(
            "while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 10 }}; \
             $child = Start-Process -FilePath 'powershell.exe' -ArgumentList \
             '-NoProfile','-Command','Start-Sleep -Seconds 300' -PassThru; \
             Set-Content -LiteralPath '{}' -Value $child.Id -NoNewline; \
             Wait-Process -Id $child.Id",
            powershell_literal(&gate),
            powershell_literal(&child_pid),
        );
        let mut parent = Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", &script])
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
        let deadline = Instant::now() + Duration::from_secs(10);
        let descendant = loop {
            if let Ok(raw_pid) = std::fs::read_to_string(&child_pid)
                && let Ok(pid) = raw_pid.parse::<u32>()
            {
                break pid;
            }
            if Instant::now() >= deadline {
                eyre::bail!("the attached parent did not create its descendant");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(process_exists(descendant)?);

        drop(job);

        let deadline = Instant::now() + Duration::from_secs(10);
        while parent.try_wait()?.is_none() || process_exists(descendant)? {
            if Instant::now() >= deadline {
                eyre::bail!("dropping the job did not terminate the full process tree");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}
