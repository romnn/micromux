//! Helpers shared by the tests that run the micromux binary.
//!
//! Every micromux a test starts keeps everything it shares per user in a private directory inside
//! the test's project: the port registry, cleanup state, control sockets, session locks, the shared
//! proxy, logs, and run-log spools.
//! A test therefore never registers with, waits for, shows up in, or writes over the developer's
//! own sessions.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The micromux binary under test.
pub const MICROMUX: &str = env!("CARGO_BIN_EXE_micromux");

/// The private state directory of the test project in `project`.
///
/// It lives inside the project's temporary directory, so it goes away with the test, and every
/// command run for the same project shares it.
/// The name is short because control sockets live inside it, and macOS limits socket paths to 104
/// bytes under its long temporary directory.
pub fn state_directory(project: &Path) -> PathBuf {
    project.join(".state")
}

/// A command running `program`, with every micromux it starts keeping its state in `project`'s
/// private state directory.
///
/// `program` is usually the micromux binary, but may be a shell that runs it.
pub fn isolated(program: impl AsRef<OsStr>, project: &Path) -> Command {
    let mut command = Command::new(program);
    command.env(micromux::STATE_DIR_VARIABLE, state_directory(project));
    command
}

/// A command running the micromux binary under test with `project`'s private state.
pub fn micromux(project: &Path) -> Command {
    isolated(MICROMUX, project)
}
