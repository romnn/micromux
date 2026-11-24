//! A session of a neighboring worktree that holds the demo's shared resource, so a scenario can
//! show the demo queueing for it.

use crate::Error;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The resource the demo's `e2e` service names, `resource` in `examples/demo/micromux.yaml`.
const RESOURCE: &str = "browser";

/// The neighbor's config: one service that needs the demo's resource.
const CONFIG: &str = "version: 1\nservices:\n  e2e:\n    command: [\"sleep\", \"1000000\"]\n    \
                      resource: browser\n";

/// A headless `micromux serve` in a sibling worktree that holds [`RESOURCE`], stopped when
/// dropped.
pub(crate) struct Neighbor {
    micromux: PathBuf,
    home: PathBuf,
    state_dir: PathBuf,
    project: PathBuf,
    child: Child,
}

impl Neighbor {
    /// Start the neighbor in `project` with `home` and `state_dir`, and wait until it holds
    /// [`RESOURCE`] with a reason the demo shows.
    pub(crate) fn start(
        micromux: &Path,
        home: &Path,
        state_dir: &Path,
        project: &Path,
    ) -> Result<Self, Error> {
        const MAX_WAIT: Duration = Duration::from_secs(15);

        std::fs::create_dir_all(project)?;
        std::fs::write(project.join("micromux.yaml"), CONFIG)?;
        let child = Command::new(micromux)
            .arg("serve")
            .current_dir(project)
            .env("HOME", home)
            .env("MICROMUX_STATE_DIR", state_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let neighbor = Self {
            micromux: micromux.to_path_buf(),
            home: home.to_path_buf(),
            state_dir: state_dir.to_path_buf(),
            project: project.to_path_buf(),
            child,
        };
        // The reservation fails until the session answers; once it does, the session holds the
        // resource, which nobody else wants yet, and records why.
        let started = Instant::now();
        while started.elapsed() < MAX_WAIT {
            let reserved = neighbor
                .ctl()
                .args([
                    "reserve",
                    RESOURCE,
                    "--reason",
                    "checkout e2e run",
                    "--for",
                    "45m",
                ])
                .status()?;
            if reserved.success() {
                return Ok(neighbor);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Err(Error::NeighborNotReady {
            project: project.to_path_buf(),
        })
    }

    /// A `micromux ctl` command for the neighbor's session.
    fn ctl(&self) -> Command {
        let mut command = Command::new(&self.micromux);
        command
            .arg("ctl")
            .current_dir(&self.project)
            .env("HOME", &self.home)
            .env("MICROMUX_STATE_DIR", &self.state_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }
}

impl Drop for Neighbor {
    /// Stops the session, which stops its service and gives the resource up, and kills it if it
    /// is still running after a few seconds.
    fn drop(&mut self) {
        if let Err(err) = self.ctl().arg("stop").status() {
            eprintln!("failed to ask the neighbor session to stop: {err}");
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if let Err(err) = self.child.kill().and_then(|()| self.child.wait().map(drop)) {
            eprintln!("failed to stop the neighbor session: {err}");
        }
    }
}
