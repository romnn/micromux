//! Job control, terminal names and process groups on Unix.

use std::io;
use std::process::Command;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::signal::unix::{SignalKind, signal};

/// The path of the terminal micromux runs in, as tmux names a pane's.
pub(crate) fn own_tty() -> Option<String> {
    let path = nix::unistd::ttyname(io::stdin()).ok()?;
    path.to_str().map(str::to_owned)
}

/// Stops micromux's whole process group, as Ctrl-Z does in a shell.
///
/// It signals the group, not just this process, so a wrapper that started micromux, such as
/// `cargo run` or `sh -c`, stops with it and the shell sees the job stop.
/// Services run in sessions and process groups of their own, so they keep running.
/// The signal is `SIGTSTP` rather than `SIGSTOP` because the system ignores `SIGTSTP` in an
/// orphaned process group, where no shell could ever continue it.
pub(crate) fn stop_process_group() -> io::Result<()> {
    // Pid 0 addresses the caller's process group.
    kill(Pid::from_raw(0), Signal::SIGTSTP).map_err(io::Error::from)
}

/// Starts `command` in a process group of its own, so Ctrl-Z, which stops micromux's group,
/// does not also stop what it started, such as a clipboard daemon that `wl-copy` or `xclip`
/// leaves serving the copied text, or a browser.
pub(crate) fn detach(command: &mut Command) {
    std::os::unix::process::CommandExt::process_group(command, 0);
}

/// Notices that the process continued after a stop, such as one sent from outside, which leaves
/// the terminal as the shell left it.
pub(crate) struct Continued {
    signal: Option<tokio::signal::unix::Signal>,
}

impl Continued {
    /// Starts watching for `SIGCONT`; without a way to watch, [`Self::recv`] never returns.
    pub(crate) fn watch() -> Self {
        let kind = SignalKind::from_raw(Signal::SIGCONT as i32);
        Self {
            signal: signal(kind).ok(),
        }
    }

    /// Waits until the process continues after a stop.
    pub(crate) async fn recv(&mut self) {
        if let Some(signal) = self.signal.as_mut()
            && signal.recv().await.is_some()
        {
            return;
        }
        std::future::pending::<()>().await;
    }
}
