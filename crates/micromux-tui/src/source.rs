//! Synchronous session data and lifecycle commands consumed by the TUI.

use micromux::{
    Command, HealthAttempt, LogLine, ServiceID, ServiceSnapshot, SessionChange, SessionModelReader,
};
use tokio::sync::{broadcast, mpsc};

/// Where the TUI's session data comes from.
///
/// The set is closed because the TUI supports only an in-process session model and the remote
/// client-side mirror added alongside attach support.
pub enum SessionSource {
    /// The in-process model of a session this process owns.
    Local(LocalSource),
}

/// An in-process session model and its lifecycle command sender.
pub struct LocalSource {
    reader: SessionModelReader,
    commands: mpsc::Sender<Command>,
}

impl LocalSource {
    /// Construct a source for a session owned by this process.
    #[must_use]
    pub fn new(reader: SessionModelReader, commands: mpsc::Sender<Command>) -> Self {
        Self { reader, commands }
    }
}

impl SessionSource {
    pub(crate) fn services(&self) -> Vec<ServiceSnapshot> {
        match self {
            Self::Local(source) => source.reader.services(),
        }
    }

    pub(crate) fn service(&self, id: &str) -> Option<ServiceSnapshot> {
        match self {
            Self::Local(source) => source.reader.service(id),
        }
    }

    pub(crate) fn logs_since(&self, id: &str, after: u64) -> (Option<u64>, Vec<LogLine>) {
        match self {
            Self::Local(source) => source.reader.logs_since(id, after),
        }
    }

    pub(crate) fn healthchecks(&self, id: &str) -> Vec<HealthAttempt> {
        match self {
            Self::Local(source) => source.reader.healthchecks(id),
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<SessionChange> {
        match self {
            Self::Local(source) => source.reader.subscribe(),
        }
    }

    pub(crate) fn restart(&self, id: ServiceID) {
        match self {
            Self::Local(source) => {
                let _ = source.commands.try_send(Command::restart(id));
            }
        }
    }

    pub(crate) fn restart_all(&self) {
        match self {
            Self::Local(source) => {
                let _ = source.commands.try_send(Command::restart_all());
            }
        }
    }

    pub(crate) fn enable(&self, id: ServiceID) {
        match self {
            Self::Local(source) => {
                let _ = source.commands.try_send(Command::enable(id));
            }
        }
    }

    pub(crate) fn disable(&self, id: ServiceID) {
        match self {
            Self::Local(source) => {
                let _ = source.commands.try_send(Command::disable(id));
            }
        }
    }

    pub(crate) fn stop_dynamic(&self, id: ServiceID) {
        match self {
            Self::Local(source) => {
                let _ = source.commands.try_send(Command::stop_dynamic(id));
            }
        }
    }
}
