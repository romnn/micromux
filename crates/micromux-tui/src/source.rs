//! Synchronous session data and lifecycle commands consumed by the TUI.

use micromux::{
    Command, HealthAttempt, LogLine, ServiceID, ServiceSnapshot, SessionChange, SessionModelReader,
};
use tokio::sync::{broadcast, mpsc};

use crate::RemoteSource;

pub(crate) struct AttachmentStatus {
    pub session: micromux_control::SessionInfo,
    pub connected: bool,
    pub notice: Option<String>,
}

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
    lifecycle: mpsc::UnboundedSender<Command>,
    notice: Option<String>,
}

impl LocalSource {
    /// Construct a source for a session owned by this process.
    #[must_use]
    pub fn new(reader: SessionModelReader, commands: mpsc::Sender<Command>) -> Self {
        let (lifecycle, mut queued) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(command) = queued.recv().await {
                if commands.send(command).await.is_err() {
                    break;
                }
            }
        });
        Self {
            reader,
            lifecycle,
            notice: None,
        }
    }

    /// Attach a persistent operator-visible warning to the local session header.
    #[must_use]
    pub fn with_notice(mut self, notice: impl Into<String>) -> Self {
        self.notice = Some(notice.into());
        self
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
                let _ = source.lifecycle.send(Command::restart(id));
            }
            Self::Remote(source) => source.restart(id),
        }
    }

    pub(crate) fn restart_all(&self) {
        match self {
            Self::Local(source) => {
                let _ = source.lifecycle.send(Command::restart_all());
            }
            Self::Remote(source) => source.restart_all(),
        }
    }

    pub(crate) fn enable(&self, id: ServiceID) {
        match self {
            Self::Local(source) => {
                let _ = source.lifecycle.send(Command::enable(id));
            }
            Self::Remote(source) => source.enable(id),
        }
    }

    pub(crate) fn disable(&self, id: ServiceID) {
        match self {
            Self::Local(source) => {
                let _ = source.lifecycle.send(Command::disable(id));
            }
            Self::Remote(source) => source.disable(id),
        }
    }

    pub(crate) fn stop_dynamic(&self, id: ServiceID) {
        match self {
            Self::Local(source) => {
                let _ = source.lifecycle.send(Command::stop_dynamic(id));
            }
            Self::Remote(source) => source.stop_dynamic(id),
        }
    }

    pub(crate) fn cancel(&self) {
        if let Self::Remote(source) = self {
            source.cancel();
        }
    }

    pub(crate) fn attachment_status(&self) -> Option<AttachmentStatus> {
        match self {
            Self::Local(_) => None,
            Self::Remote(source) => Some(source.attachment_status()),
        }
    }

    pub(crate) fn local_notice(&self) -> Option<&str> {
        match self {
            Self::Local(source) => source.notice.as_deref(),
            Self::Remote(_) => None,
        }
    }
}
