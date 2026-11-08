//! The platform interface where there is no Unix job control, such as on Windows.

use std::io;
use std::process::Command;

/// The path of the terminal micromux runs in, which only Unix names.
pub(crate) fn own_tty() -> Option<String> {
    None
}

/// Fails, since there is no job control to stop micromux with.
///
/// The TUI only asks for a suspend on Unix, so this is never reached.
pub(crate) fn stop_process_group() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "suspending needs Unix job control",
    ))
}

/// Leaves `command` as it is, since Ctrl-Z stops no process group here.
pub(crate) fn detach(_command: &mut Command) {}

/// Never notices a continue, since nothing here stops the process the way Unix job control does.
pub(crate) struct Continued;

impl Continued {
    /// Starts a watch that never fires.
    pub(crate) fn watch() -> Self {
        Self
    }

    /// Never returns.
    pub(crate) async fn recv(&mut self) {
        std::future::pending::<()>().await;
    }
}
