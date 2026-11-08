//! Capture README screenshots of the real micromux TUI.
//!
//! This crate is never published. It deliberately uses **no micromux internals**: it runs the
//! actual `micromux` binary inside a pseudo-terminal, lets the UI settle, scrapes the rendered
//! screen with a `vt100` terminal emulator, and renders the result to a PNG with
//! [`freeze`](https://github.com/charmbracelet/freeze). Driving the published binary the same way a
//! user would keeps the screenshots honest and keeps all of this tooling out of the published
//! crates.

mod ansi;
mod capture;
mod freeze;
mod proxy;
mod scenario;
mod theme;

use crate::proxy::Proxy;
use crate::scenario::SCENARIOS;
use crate::theme::THEMES;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{operation}: {message}")]
    Operation {
        operation: &'static str,
        message: String,
    },
    #[error("current executable has no parent directory")]
    MissingExecutableParent,
    #[error(
        "micromux binary not found at {}; build it with `cargo build -p micromux-cli` or set MICROMUX_BIN\nworkspace: {}",
        candidate.display(),
        workspace.display()
    )]
    BinaryNotFound {
        candidate: PathBuf,
        workspace: PathBuf,
    },
    #[error("`freeze` not found; install it from https://github.com/charmbracelet/freeze")]
    FreezeMissing,
    #[error(
        "micromux exited before capture (status: {status:?}); check the demo config in {}",
        example_dir.display()
    )]
    EarlyExit {
        status: portable_pty::ExitStatus,
        example_dir: PathBuf,
    },
    #[error(
        "micromux drew no screen in time; check the demo config in {}",
        example_dir.display()
    )]
    NoFirstFrame { example_dir: PathBuf },
    #[error("freeze failed for {}", input.display())]
    FreezeFailed { input: PathBuf },
    #[error(
        "port {port} for the demo proxy is taken; a proxy of an earlier run exits within a minute"
    )]
    ProxyPortTaken { port: u16 },
    #[error("the demo proxy did not listen on port {port} in time")]
    ProxyNotReady { port: u16 },
}

impl Error {
    pub(crate) fn operation(operation: &'static str, error: impl std::fmt::Display) -> Self {
        Self::Operation {
            operation,
            message: error.to_string(),
        }
    }
}

fn main() -> Result<(), Error> {
    let workspace = workspace_dir()?;
    let micromux = micromux_bin(&workspace)?;
    // Stage the demo under a scratch home at the path of the main checkout, so the screenshots
    // show the same directory whichever checkout or worktree renders them.
    let home =
        std::env::temp_dir().join(format!("micromux-screenshot-home-{}", std::process::id()));
    std::fs::create_dir_all(&home)?;
    // micromux shows real paths, so the home must be one too, which a temp dir is not on macOS.
    let home = std::fs::canonicalize(&home)?;
    let example_dir = home
        .join("dev")
        .join("micromux")
        .join("examples")
        .join("demo");
    copy_dir(&workspace.join("examples").join("demo"), &example_dir)?;
    // The screenshots live in the Hugo static tree so the docs site and the README can both serve
    // them (README via a raw.githubusercontent URL, the site as /images/<name>.png).
    let images = workspace.join("docs").join("static").join("images");

    freeze::ensure_freeze()?;
    std::fs::create_dir_all(&images)?;

    // Keep the demo's sessions, ports and proxy apart from the sessions running on this machine.
    let state_dir = home.join("state");
    // Each scenario's session would start a proxy that outlives it by a minute and keeps the next
    // run from binding the port, so the demo runs one proxy of its own for all of them.
    let proxy = Proxy::start(&micromux, &state_dir)?;

    for scenario in SCENARIOS {
        let screen = capture::capture(&micromux, &home, &state_dir, &example_dir, scenario)?;

        for theme in &THEMES {
            let ansi =
                ansi::screen_to_ansi(&screen, scenario.rows.get(), scenario.cols.get(), theme);
            let name = format!("{}{}", scenario.name, theme.suffix);
            let tmp = std::env::temp_dir().join(format!("micromux-screenshot-{name}.ansi"));
            std::fs::write(&tmp, &ansi)?;

            let png = images.join(format!("{name}.png"));
            freeze::run_freeze(&tmp, &png, theme)?;
            let _ = std::fs::remove_file(&tmp);
            println!("wrote {}", png.display());
        }
    }

    drop(proxy);
    let _ = std::fs::remove_dir_all(&home);
    Ok(())
}

/// Copy the directory tree at `from` to `to`, keeping file permissions.
fn copy_dir(from: &Path, to: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Absolute path to the workspace root (this crate lives at `crates/micromux-screenshot`).
fn workspace_dir() -> Result<PathBuf, Error> {
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()?)
}

/// Locate the `micromux` binary: `$MICROMUX_BIN` if set, otherwise the sibling of this tool's own
/// executable (the same `target/<profile>` directory).
fn micromux_bin(workspace: &Path) -> Result<PathBuf, Error> {
    if let Some(path) = std::env::var_os("MICROMUX_BIN") {
        return Ok(PathBuf::from(path));
    }

    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or(Error::MissingExecutableParent)?;
    let name = if cfg!(windows) {
        "micromux.exe"
    } else {
        "micromux"
    };
    let candidate = dir.join(name);
    if candidate.exists() {
        return Ok(candidate);
    }

    Err(Error::BinaryNotFound {
        candidate,
        workspace: workspace.to_path_buf(),
    })
}
