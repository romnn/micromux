//! A session claims its config before it waits for anything, exercised through the real binary.
#![cfg(unix)]

mod common;

use std::io::BufRead as _;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use color_eyre::eyre;

/// A `micromux serve` for the project in `directory`, with its stderr piped.
fn serve(directory: &std::path::Path) -> eyre::Result<Child> {
    Ok(common::micromux(directory)
        .arg("serve")
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?)
}

/// Stops `child` when the test ends, however it ends.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A second start for a config whose session is still starting, here waiting for a previous
/// session's cleanup, fails at once and names that session's process, instead of waiting
/// alongside it and starting the same services.
#[test]
fn a_second_start_for_a_starting_config_fails_at_once() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let config_path = directory.path().join("micromux.yaml");
    std::fs::write(
        &config_path,
        "version: 1\nservices:\n  svc:\n    command: [\"sh\", \"-c\", \"exec sleep 300\"]\n",
    )?;

    // This test process stands in for a shim of a previous session whose cleanup still runs.
    let cleanups = micromux::cleanup_directory_in(
        &common::state_directory(directory.path()),
        &config_path.canonicalize()?,
    );
    std::fs::create_dir_all(&cleanups)?;
    let cleanup_lock = std::fs::File::create(cleanups.join("svc.lock"))?;
    cleanup_lock.lock()?;

    // The first start claims the config and then waits for that cleanup.
    let mut first = KillOnDrop(serve(directory.path())?);
    let first_pid = first.0.id();
    let stderr = first
        .0
        .stderr
        .take()
        .ok_or_else(|| eyre::eyre!("no stderr"))?;
    let mut lines = std::io::BufReader::new(stderr).lines();
    loop {
        let line = lines
            .next()
            .ok_or_else(|| eyre::eyre!("the first start ended before it waited"))??;
        if line.contains(micromux::WAITING_FOR_PREVIOUS_CLEANUP) {
            break;
        }
    }

    // The second start fails without waiting, naming the first.
    let mut second = serve(directory.path())?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = second.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            second.kill()?;
            second.wait()?;
            eyre::bail!("the second start did not fail at once");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut output = String::new();
    if let Some(mut pipe) = second.stderr.take() {
        std::io::Read::read_to_string(&mut pipe, &mut output)?;
    }
    assert!(!status.success(), "{output}");
    assert!(
        output.contains(micromux_control::SESSION_ALREADY_STARTING),
        "{output}"
    );
    assert!(output.contains(&format!("(pid {first_pid})")), "{output}");
    assert!(
        !output.contains(micromux::WAITING_FOR_PREVIOUS_CLEANUP),
        "{output}"
    );
    Ok(())
}
