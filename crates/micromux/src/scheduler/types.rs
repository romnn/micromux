//! Scheduler command and lifecycle types.

use super::control::{Caller, ReplyAck, ResolvedEnvironment, ServiceCommandAck};
use crate::cleanup::CleanupOutcome;
use crate::health_check::Health;
use crate::session::ReservationReply;
use crate::{DynamicServiceParams, Lease};

/// Unique identifier for a service.
pub type ServiceID = String;

/// Largest raw PTY input payload accepted as one all-or-nothing write batch.
pub const MAX_PTY_INPUT_BATCH_BYTES: usize = 64 * 1024;
/// Largest bracketed paste accepted as one logical PTY write.
pub const MAX_PTY_PASTE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct RunId(u64);

impl RunId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The underlying monotonic value, surfaced publicly as `run_generation`.
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

/// The lifecycle state of a service.
#[derive(Debug, Clone)]
pub(crate) enum State {
    /// Service has not yet started.
    Pending,
    /// Service is currently starting.
    Starting,
    /// Service is running.
    Running { health: Option<Health> },
    /// Service is disabled.
    Disabled,
    /// Service exited with code.
    Exited { exit_code: i32 },
    /// Service has been killed and is awaiting exit.
    Killed,
}

#[derive(Debug, Clone)]
pub(crate) enum ProcessEvent {
    Killed {
        service_id: ServiceID,
        run_id: RunId,
    },
    /// The run's process exited and its cleanup started; `Exited` follows once it ends.
    CleanupStarted {
        service_id: ServiceID,
        run_id: RunId,
        exit_code: i32,
    },
    Exited {
        service_id: ServiceID,
        run_id: RunId,
        exit_code: i32,
        /// How the run's cleanup ended, for a service with one.
        cleanup: Option<CleanupOutcome>,
    },
    LogReaderFinished {
        service_id: ServiceID,
        run_id: RunId,
    },
    Healthy {
        service_id: ServiceID,
        run_id: RunId,
    },
    Unhealthy {
        service_id: ServiceID,
        run_id: RunId,
    },
    InputDropped {
        service_id: ServiceID,
        run_id: RunId,
        input_kind: &'static str,
        reason: String,
    },
}

impl ProcessEvent {
    pub(crate) fn service_id(&self) -> &ServiceID {
        match self {
            Self::Killed { service_id, .. }
            | Self::CleanupStarted { service_id, .. }
            | Self::Exited { service_id, .. }
            | Self::LogReaderFinished { service_id, .. }
            | Self::Healthy { service_id, .. }
            | Self::Unhealthy { service_id, .. }
            | Self::InputDropped { service_id, .. } => service_id,
        }
    }

    pub(crate) fn run_id(&self) -> RunId {
        match self {
            Self::Killed { run_id, .. }
            | Self::CleanupStarted { run_id, .. }
            | Self::Exited { run_id, .. }
            | Self::LogReaderFinished { run_id, .. }
            | Self::Healthy { run_id, .. }
            | Self::Unhealthy { run_id, .. }
            | Self::InputDropped { run_id, .. } => *run_id,
        }
    }

    #[cfg(test)]
    pub(crate) fn to_test_event(&self) -> Event {
        match self {
            Self::Killed { service_id, .. } => Event::Killed(service_id.clone()),
            Self::CleanupStarted { service_id, .. } => Event::CleanupStarted(service_id.clone()),
            Self::Exited {
                service_id,
                exit_code,
                ..
            } => Event::Exited(service_id.clone(), *exit_code),
            Self::LogReaderFinished { service_id, .. } => {
                Event::LogReaderFinished(service_id.clone())
            }
            Self::Healthy { service_id, .. } => Event::Healthy(service_id.clone()),
            Self::Unhealthy { service_id, .. } => Event::Unhealthy(service_id.clone()),
            Self::InputDropped { service_id, .. } => Event::InputDropped(service_id.clone()),
        }
    }
}

#[cfg(test)]
#[cfg_attr(
    not(unix),
    expect(
        dead_code,
        reason = "test event payloads are asserted by Unix PTY tests; unsupported-platform test builds keep the shared type"
    )
)]
#[derive(Debug)]
pub(crate) enum Event {
    /// A service has started.
    Started {
        /// Service that started.
        service_id: ServiceID,
    },
    /// A service was killed.
    Killed(ServiceID),
    /// A service's process exited and its cleanup started.
    CleanupStarted(ServiceID),
    /// A service exited.
    Exited(ServiceID, i32),
    /// A service's PTY reader drained and exited.
    LogReaderFinished(ServiceID),
    /// A service became healthy.
    Healthy(ServiceID),
    /// A service became unhealthy.
    Unhealthy(ServiceID),
    /// Terminal input was discarded while writing to the service.
    InputDropped(ServiceID),
    /// A service was disabled.
    Disabled(ServiceID),
    /// Clear the log buffer for a service (e.g. on restart).
    ClearLogs(ServiceID),
}

/// The kind of log update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogUpdateKind {
    /// Append a new line to the log buffer.
    Append,
    /// Update a live snapshot line, appending it first if the target line is absent.
    LiveSnapshot {
        /// Stable identifier for the live snapshot line within one process run.
        id: u64,
    },
}

/// Origin stream of output.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
    /// A newer peer sent an output stream this binary does not know yet.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Started { service_id, .. } => write!(f, "Started({service_id})"),
            Self::Killed(service_id) => write!(f, "Killed({service_id})"),
            Self::CleanupStarted(service_id) => write!(f, "CleanupStarted({service_id})"),
            Self::Exited(service_id, _) => write!(f, "Exited({service_id})"),
            Self::LogReaderFinished(service_id) => write!(f, "LogReaderFinished({service_id})"),
            Self::Healthy(service_id) => write!(f, "Healthy({service_id})"),
            Self::Unhealthy(service_id) => write!(f, "Unhealthy({service_id})"),
            Self::InputDropped(service_id) => write!(f, "InputDropped({service_id})"),
            Self::Disabled(service_id) => write!(f, "Disabled({service_id})"),
            Self::ClearLogs(service_id) => write!(f, "ClearLogs({service_id})"),
        }
    }
}

/// A change to this session's request for a resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReservationRequest {
    /// Reserve the resource, or update and renew this session's request for it.
    Reserve {
        /// The resource.
        resource: String,
        /// How long the caller expects to hold it, for display.
        estimate_secs: Option<u64>,
        /// Why the caller wants it, for display.
        reason: Option<String>,
    },
    /// Renew this session's request for the resource.
    Renew {
        /// The resource.
        resource: String,
        /// Renew only this request.
        claim: Option<String>,
    },
    /// Release this session's request for the resource.
    Release {
        /// The resource.
        resource: String,
    },
    /// Report where this session's request for the resource stands, changing nothing.
    Status {
        /// The resource.
        resource: String,
    },
}

/// A command sent to the scheduler.
///
/// Acknowledged variants carry an optional reply half: the trusted in-process TUI passes `None`
/// (fire-and-forget), while the narrow [`super::ServiceControl`] port attaches an ack so the
/// scheduler validates and replies request/response.
/// Every mutating variant names its [`Caller`]; the constructors below act as
/// [`Caller::Local`]. `Command` is not `Clone` because an ack is
/// single-shot.
#[derive(Debug)]
pub enum Command {
    /// Restart a single service.
    Restart {
        /// Who sent the command.
        caller: Caller,
        /// Service to restart.
        service: ServiceID,
        /// Optional reply channel for acknowledged commands.
        ack: Option<ReplyAck<Vec<ServiceCommandAck>>>,
    },
    /// Restart all enabled services.
    RestartAll {
        /// Who sent the command.
        caller: Caller,
        /// Optional reply channel for acknowledged commands.
        ack: Option<ReplyAck<Vec<ServiceCommandAck>>>,
    },
    /// Disable a single service.
    Disable {
        /// Who sent the command.
        caller: Caller,
        /// Service to disable.
        service: ServiceID,
        /// Optional reply channel for acknowledged commands.
        ack: Option<ReplyAck<Vec<ServiceCommandAck>>>,
    },
    /// Enable a single service.
    Enable {
        /// Who sent the command.
        caller: Caller,
        /// Service to enable.
        service: ServiceID,
        /// Optional reply channel for acknowledged commands.
        ack: Option<ReplyAck<Vec<ServiceCommandAck>>>,
    },
    /// Reconcile configured services against the on-disk config.
    ReconcileConfig {
        /// Who sent the command.
        caller: Caller,
        /// Compute the semantic diff without mutating the session.
        dry_run: bool,
        /// Required reply channel.
        ack: ReplyAck<super::control::ReconcileReceipt>,
    },
    /// Create a dynamic service.
    StartDynamic {
        /// Who sent the command.
        caller: Caller,
        /// Definition and lease request.
        params: DynamicServiceParams,
        /// Optional reply channel for acknowledged commands.
        ack: Option<ReplyAck<super::control::DynamicServiceAck>>,
    },
    /// Replace or revive a dynamic service.
    ReplaceDynamic {
        /// Who sent the command.
        caller: Caller,
        /// Existing dynamic service id.
        service: ServiceID,
        /// Required current revision.
        expected_revision: u64,
        /// Replacement definition and lease request.
        params: DynamicServiceParams,
        /// Optional reply channel for acknowledged commands.
        ack: Option<ReplyAck<super::control::DynamicServiceAck>>,
    },
    /// Renew a live dynamic service's lease without restarting it.
    RenewDynamic {
        /// Who sent the command.
        caller: Caller,
        /// Existing dynamic service id.
        service: ServiceID,
        /// Required current revision.
        expected_revision: u64,
        /// Requested replacement lease.
        expires_after: Option<Lease>,
        /// Optional reply channel for acknowledged commands.
        ack: Option<ReplyAck<super::control::DynamicServiceAck>>,
    },
    /// Retire a dynamic service.
    StopDynamic {
        /// Who sent the command.
        caller: Caller,
        /// Dynamic service id.
        service: ServiceID,
        /// Optional reply channel for acknowledged commands.
        ack: Option<ReplyAck<super::control::DynamicServiceAck>>,
    },
    /// Report the session's variables, or one service's resolved environment.
    Environment {
        /// Service id, or `None` for the session's variables.
        service: Option<ServiceID>,
        /// Required reply channel.
        ack: ReplyAck<ResolvedEnvironment>,
    },
    /// Report the session's own environment: the session-wide `env_file` and the session's
    /// variables over it.
    SessionEnvironment {
        /// Required reply channel.
        ack: ReplyAck<ResolvedEnvironment>,
    },
    /// Reserve, renew, or release a resource for this session.
    Reservation {
        /// Who sent the command.
        caller: Caller,
        /// What to do with which resource.
        request: ReservationRequest,
        /// Required reply channel.
        ack: ReplyAck<ReservationReply>,
    },
    /// Stop the session for a caller, unless it may not.
    Shutdown {
        /// Who asks.
        caller: Caller,
        /// Required reply channel.
        ack: ReplyAck<()>,
    },
    /// Resize all PTYs.
    ResizeAll {
        /// Terminal width in columns.
        cols: u16,
        /// Terminal height in rows.
        rows: u16,
    },
}

impl Command {
    /// Fire-and-forget restart (no acknowledgement).
    #[must_use]
    pub fn restart(service: ServiceID) -> Self {
        Self::Restart {
            caller: Caller::Local,
            service,
            ack: None,
        }
    }

    /// Fire-and-forget restart-all (no acknowledgement).
    #[must_use]
    pub fn restart_all() -> Self {
        Self::RestartAll {
            caller: Caller::Local,
            ack: None,
        }
    }

    /// Fire-and-forget disable (no acknowledgement).
    #[must_use]
    pub fn disable(service: ServiceID) -> Self {
        Self::Disable {
            caller: Caller::Local,
            service,
            ack: None,
        }
    }

    /// Fire-and-forget enable (no acknowledgement).
    #[must_use]
    pub fn enable(service: ServiceID) -> Self {
        Self::Enable {
            caller: Caller::Local,
            service,
            ack: None,
        }
    }

    /// Fire-and-forget retirement of a dynamic service.
    #[must_use]
    pub fn stop_dynamic(service: ServiceID) -> Self {
        Self::StopDynamic {
            caller: Caller::Local,
            service,
            ack: None,
        }
    }
}
