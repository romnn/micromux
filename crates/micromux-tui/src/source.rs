//! Synchronous session data and lifecycle commands consumed by the TUI.

use micromux::{
    Command, HealthAttempt, LogLine, ServiceID, ServiceSnapshot, SessionChange, SessionModelReader,
};
use tokio::sync::{broadcast, mpsc};

use crate::RemoteSource;

/// Where the TUI's session data comes from.
///
/// The set is closed because the TUI supports only an in-process session model and the remote
/// client-side mirror added alongside attach support.
pub enum SessionSource {
    /// The in-process model of a session this process owns.
    Local(LocalSource),
    /// A synchronous mirror of a session reached through its control endpoint.
    Remote(RemoteSource),
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
            Self::Remote(source) => source.services(),
        }
    }

    pub(crate) fn service(&self, id: &str) -> Option<ServiceSnapshot> {
        match self {
            Self::Local(source) => source.reader.service(id),
            Self::Remote(source) => source.service(id),
        }
    }

    pub(crate) fn logs_since(&self, id: &str, after: u64) -> (Option<u64>, Vec<LogLine>) {
        match self {
            Self::Local(source) => source.reader.logs_since(id, after),
            Self::Remote(source) => source.logs_since(id, after),
        }
    }

    pub(crate) fn healthchecks(&self, id: &str) -> Vec<HealthAttempt> {
        match self {
            Self::Local(source) => source.reader.healthchecks(id),
            Self::Remote(source) => source.healthchecks(id),
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<SessionChange> {
        match self {
            Self::Local(source) => source.reader.subscribe(),
            Self::Remote(source) => source.subscribe(),
        }
    }

    pub(crate) fn restart(&self, id: ServiceID) {
        match self {
            Self::Local(source) => {
                let _ = source.commands.try_send(Command::restart(id));
            }
            Self::Remote(source) => source.restart(id),
        }
    }

    pub(crate) fn restart_all(&self) {
        match self {
            Self::Local(source) => {
                let _ = source.commands.try_send(Command::restart_all());
            }
            Self::Remote(source) => source.restart_all(),
        }
    }

    pub(crate) fn enable(&self, id: ServiceID) {
        match self {
            Self::Local(source) => {
                let _ = source.commands.try_send(Command::enable(id));
            }
            Self::Remote(source) => source.enable(id),
        }
    }

    pub(crate) fn disable(&self, id: ServiceID) {
        match self {
            Self::Local(source) => {
                let _ = source.commands.try_send(Command::disable(id));
            }
            Self::Remote(source) => source.disable(id),
        }
    }

    pub(crate) fn stop_dynamic(&self, id: ServiceID) {
        match self {
            Self::Local(source) => {
                let _ = source.commands.try_send(Command::stop_dynamic(id));
            }
            Self::Remote(source) => source.stop_dynamic(id),
        }
    }

    pub(crate) fn cancel(&self) {
        if let Self::Remote(source) = self {
            source.cancel();
        }
    }
}
