//! Synchronous session data and lifecycle commands consumed by the TUI.

use std::sync::Arc;

use micromux::{
    Caller, Command, CommandRejection, HealthAttempt, LogLine, SchedulerStopped, ServiceControl,
    ServiceEvent, ServiceID, ServiceSnapshot, SessionChange, SessionModelReader, SessionStatus,
    SessionSummary,
};
use parking_lot::Mutex;
use tokio::sync::{Notify, broadcast, mpsc};

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

/// A lifecycle command of the local TUI.
enum LocalCommand {
    Restart(ServiceID),
    RestartAll,
    Enable(ServiceID),
    Disable(ServiceID),
    StopDynamic(ServiceID),
}

/// An in-process session model and its lifecycle command sender.
pub struct LocalSource {
    reader: SessionModelReader,
    lifecycle: mpsc::UnboundedSender<LocalCommand>,
    notice: Option<String>,
    /// Why the latest lifecycle command was refused, until one is accepted.
    refusal: Arc<Mutex<Option<String>>>,
    /// Wakes the TUI when `refusal` changed.
    refusal_changed: Arc<Notify>,
    session: Option<SessionStatus>,
}

impl LocalSource {
    /// Construct a source for a session owned by this process.
    ///
    /// Its lifecycle commands run one after another; a refused one's message shows in the header
    /// until a later command is accepted.
    #[must_use]
    pub fn new(reader: SessionModelReader, commands: mpsc::Sender<Command>) -> Self {
        let (lifecycle, mut queued) = mpsc::unbounded_channel();
        let refusal = Arc::new(Mutex::new(None));
        let refusal_changed = Arc::new(Notify::new());
        let control = ServiceControl::from(commands);
        tokio::spawn({
            let refusal = Arc::clone(&refusal);
            let refusal_changed = Arc::clone(&refusal_changed);
            async move {
                while let Some(command) = queued.recv().await {
                    let Ok(refused) = run_local(&control, command).await else {
                        break;
                    };
                    let mut current = refusal.lock();
                    if *current != refused {
                        *current = refused;
                        refusal_changed.notify_one();
                    }
                }
            }
        });
        Self {
            reader,
            lifecycle,
            notice: None,
            refusal,
            refusal_changed,
            session: None,
        }
    }

    /// Shows the session's URLs in the header.
    #[must_use]
    pub fn with_session(mut self, session: SessionStatus) -> Self {
        self.session = Some(session);
        self
    }

    /// Attach a persistent operator-visible warning to the local session header.
    #[must_use]
    pub fn with_notice(mut self, notice: impl Into<String>) -> Self {
        self.notice = Some(notice.into());
        self
    }
}

/// Runs `command` as the session's own process and returns why the scheduler refused it, if it
/// did.
async fn run_local(
    control: &ServiceControl,
    command: LocalCommand,
) -> Result<Option<String>, SchedulerStopped> {
    let refused = |rejection: CommandRejection| Some(rejection.message());
    Ok(match command {
        LocalCommand::Restart(id) => control.restart(Caller::Local, &id).await?.err(),
        LocalCommand::RestartAll => control.restart_all(Caller::Local).await?.err(),
        LocalCommand::Enable(id) => control.enable(Caller::Local, &id).await?.err(),
        LocalCommand::Disable(id) => control.disable(Caller::Local, &id).await?.err(),
        LocalCommand::StopDynamic(id) => control.stop_dynamic(Caller::Local, &id).await?.err(),
    }
    .and_then(refused))
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

    /// A service's retained lifecycle events, oldest first; empty for an unknown service.
    ///
    /// Both sources read an in-process store of at most [`micromux::EVENT_HISTORY`] events: the
    /// session model, or the attached TUI's mirror of it.
    /// It is `None` when the session reports no lifecycle events, as an attached session older
    /// than [`micromux_control::EVENTS_PROTOCOL_VERSION`].
    pub(crate) fn events(&self, id: &str) -> Option<Vec<ServiceEvent>> {
        match self {
            Self::Local(source) => Some(source.reader.events(id, None, None).0),
            Self::Remote(source) => source.events(id),
        }
    }

    pub(crate) fn latest_event_seq(&self, id: &str) -> Option<u64> {
        let Self::Local(source) = self else {
            return None;
        };
        source
            .reader
            .events(id, None, Some(1))
            .0
            .pop()
            .map(|event| event.seq)
    }

    /// Returns the advanced cursor and the newest input drop not already covered by `after`.
    ///
    /// A cursor may move backward when a removed service id is reused for a new model entry.
    pub(crate) fn input_drop_notice_since(
        &self,
        id: &str,
        after: Option<u64>,
    ) -> (Option<u64>, Option<String>) {
        let Self::Local(source) = self else {
            return (after, None);
        };
        let latest = source
            .reader
            .events(id, None, Some(1))
            .0
            .pop()
            .map(|event| event.seq);
        if after.is_some_and(|after| latest.is_none_or(|latest| after > latest)) {
            return (latest, None);
        }

        let events = source.reader.events(id, after, None).0;
        let cursor = events.last().map(|event| event.seq).or(after);
        let notice = events
            .into_iter()
            .rev()
            .find(|event| event.kind == micromux::ServiceEventKind::InputDropped)
            .map(|event| event.detail);
        (cursor, notice)
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
                let _ = source.lifecycle.send(LocalCommand::Restart(id));
            }
            Self::Remote(source) => source.restart(id),
        }
    }

    pub(crate) fn restart_all(&self) {
        match self {
            Self::Local(source) => {
                let _ = source.lifecycle.send(LocalCommand::RestartAll);
            }
            Self::Remote(source) => source.restart_all(),
        }
    }

    pub(crate) fn enable(&self, id: ServiceID) {
        match self {
            Self::Local(source) => {
                let _ = source.lifecycle.send(LocalCommand::Enable(id));
            }
            Self::Remote(source) => source.enable(id),
        }
    }

    pub(crate) fn disable(&self, id: ServiceID) {
        match self {
            Self::Local(source) => {
                let _ = source.lifecycle.send(LocalCommand::Disable(id));
            }
            Self::Remote(source) => source.disable(id),
        }
    }

    pub(crate) fn stop_dynamic(&self, id: ServiceID) {
        match self {
            Self::Local(source) => {
                let _ = source.lifecycle.send(LocalCommand::StopDynamic(id));
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

    /// Follows the local session's URLs and routing status; a remote source reports changes through
    /// its regular change stream instead.
    pub(crate) fn summary_changes(&self) -> Option<tokio::sync::watch::Receiver<SessionSummary>> {
        match self {
            Self::Local(source) => source.session.as_ref().map(SessionStatus::subscribe),
            Self::Remote(_) => None,
        }
    }

    /// The session's name, URLs, and ports, when the source knows them.
    pub(crate) fn session_summary(&self) -> Option<SessionSummary> {
        match self {
            Self::Local(source) => source.session.as_ref().map(SessionStatus::current),
            Self::Remote(source) => source
                .attachment_status()
                .session
                .session
                .map(|session| *session),
        }
    }

    /// Whether the session left its reservations out of a description too large to send.
    pub(crate) fn reservations_truncated(&self) -> bool {
        match self {
            Self::Local(_) => false,
            Self::Remote(source) => source.attachment_status().session.reservations_truncated,
        }
    }

    /// Holder and queue of every resource the session uses, read now for a local session and
    /// as last described for a remote one.
    pub(crate) fn reservations(&self) -> Vec<micromux::ResourceView> {
        match self {
            Self::Local(source) => source
                .session
                .as_ref()
                .map(SessionStatus::reservations)
                .unwrap_or_default(),
            Self::Remote(_) => self
                .session_summary()
                .map(|session| session.reservations)
                .unwrap_or_default(),
        }
    }

    /// The local session's notice: why its latest lifecycle command was refused, else its
    /// persistent warning.
    pub(crate) fn local_notice(&self) -> Option<String> {
        match self {
            Self::Local(source) => source
                .refusal
                .lock()
                .clone()
                .or_else(|| source.notice.clone()),
            Self::Remote(_) => None,
        }
    }

    /// Waits until a local lifecycle command's refusal changed; never for a remote session,
    /// whose notice arrives as a session change.
    pub(crate) async fn local_notice_changed(&self) {
        match self {
            Self::Local(source) => source.refusal_changed.notified().await,
            Self::Remote(_) => std::future::pending().await,
        }
    }
}
