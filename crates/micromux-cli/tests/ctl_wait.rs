//! `micromux ctl wait`, exercised through the real binary against a headless session.
#![cfg(unix)]

use std::path::Path;
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use color_eyre::eyre;
use similar_asserts::assert_eq;
use std::os::unix::process::CommandExt as _;

mod common;

const CONFIG: &str = r#"version: 1
services:
  seed:
    command: ["sh", "-c", "exit 3"]
    restart: "no"
  api:
    command: ["sh", "-c", "exec sleep 300"]
    depends_on:
      - name: seed
        condition: completed
  web:
    command: ["sh", "-c", "exec sleep 300"]
  off:
    command: ["sh", "-c", "exec sleep 300"]
    disabled: true
"#;

fn ctl(directory: &Path, args: &[&str]) -> eyre::Result<Output> {
    Ok(common::micromux(directory)
        .arg("ctl")
        .args(args)
        .current_dir(directory)
        .stdin(Stdio::null())
        .output()?)
}

/// Stops the session when the test ends, however it ends.
struct Session {
    directory: tempfile::TempDir,
    supervisor: std::process::Child,
}

impl Session {
    fn start(config: &str) -> eyre::Result<Self> {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("micromux.yaml"), config)?;
        let supervisor = common::micromux(directory.path())
            .arg("serve")
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let session = Self {
            directory,
            supervisor,
        };
        // The control endpoint comes up shortly after the process starts.
        let deadline = Instant::now() + Duration::from_secs(30);
        while !ctl(session.directory.path(), &["ls"])?.status.success() {
            eyre::ensure!(
                Instant::now() < deadline,
                "the session never answered `ctl ls`"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(session)
    }

    fn wait(&self, args: &[&str]) -> eyre::Result<(Option<i32>, String)> {
        let output = ctl(self.directory.path(), &[&["wait"], args].concat())?;
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok((output.status.code(), text))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = ctl(self.directory.path(), &["stop"]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.supervisor.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.supervisor.kill();
        let _ = self.supervisor.wait();
    }
}

/// Every way a wait can end has its own exit code, and a wait behind a dependency that will not
/// restart fails at once instead of running into its timeout.
#[test]
fn ctl_wait_exit_codes_tell_the_outcomes_apart() -> eyre::Result<()> {
    let session = Session::start(CONFIG)?;

    let (code, text) = session.wait(&["web", "--timeout", "30s"])?;
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("web is healthy"), "{text}");

    let (code, text) = session.wait(&["seed", "--exit", "--timeout", "30s"])?;
    assert_eq!(code, Some(3), "{text}");
    assert!(text.contains("seed exited with code 3"), "{text}");

    let started = Instant::now();
    let (code, text) = session.wait(&["api", "--healthy", "--timeout", "60s"])?;
    assert_eq!(code, Some(4), "{text}");
    assert!(
        text.contains("dependency `seed` exited with code 3 and will not restart"),
        "{text}"
    );
    let (code, text) = session.wait(&["api", "--exit", "--timeout", "60s"])?;
    assert_eq!(code, Some(4), "{text}");
    assert!(started.elapsed() < Duration::from_secs(30));

    let (code, text) = session.wait(&["off"])?;
    assert_eq!(code, Some(5), "{text}");
    assert!(text.contains("off is disabled"), "{text}");

    let (code, text) = session.wait(&["web", "--exit", "--timeout", "300ms"])?;
    assert_eq!(code, Some(124), "{text}");
    assert!(text.contains("timed out after 300ms"), "{text}");

    let listed = ctl(session.directory.path(), &["ls"])?;
    assert!(
        String::from_utf8_lossy(&listed.stdout)
            .contains("failed_dependency=seed(exited,exit_code=3)")
    );
    Ok(())
}

const JOBS_CONFIG: &str = r#"version: 1
services:
  seed:
    kind: job
    command: ["sh", "-c", "sleep 0.2"]
  reset:
    kind: job
    run: manual
    command: ["sh", "-c", "exit 0"]
  migrate:
    kind: job
    command: ["sh", "-c", "exit 7"]
  api:
    command: ["sh", "-c", "exec sleep 300"]
    depends_on: [seed]
"#;

/// A completed job is a successful wait, a failed one an exit, and a manual job that never ran
/// fails fast until a restart runs it.
#[test]
fn ctl_wait_treats_a_completed_job_as_ready() -> eyre::Result<()> {
    let session = Session::start(JOBS_CONFIG)?;

    let (code, text) = session.wait(&["seed", "--timeout", "30s"])?;
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("seed completed"), "{text}");
    let (code, text) = session.wait(&["api", "--timeout", "30s"])?;
    assert_eq!(code, Some(0), "{text}");

    let (code, text) = session.wait(&["migrate", "--timeout", "30s"])?;
    assert_eq!(code, Some(3), "{text}");
    assert!(text.contains("migrate exited with code 7"), "{text}");

    let (code, text) = session.wait(&["reset", "--timeout", "30s"])?;
    assert_eq!(code, Some(5), "{text}");
    assert!(text.contains("manual job that has not run"), "{text}");

    let restarted = ctl(session.directory.path(), &["restart", "reset"])?;
    assert!(restarted.status.success());
    let (code, text) = session.wait(&["reset", "--after-generation", "0", "--timeout", "30s"])?;
    assert_eq!(code, Some(0), "{text}");

    let listed =
        String::from_utf8_lossy(&ctl(session.directory.path(), &["ls"])?.stdout).into_owned();
    assert!(
        listed.contains("kind=job run=manual desired=Enabled execution=Completed"),
        "{listed}"
    );
    assert!(listed.contains("execution=Failed exit_code=7"), "{listed}");
    Ok(())
}
