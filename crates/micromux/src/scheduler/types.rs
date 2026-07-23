//! Scheduler command and lifecycle types.

use super::control::{CommandAck, DynamicCommandAck};
use crate::health_check::Health;
use crate::{DynamicServiceParams, Lease};

/// Unique identifier for a service.
pub type ServiceID = String;

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

#[derive(Debug)]
pub(crate) enum ProcessEvent {
    Killed {
        service_id: ServiceID,
        run_id: RunId,
    },
    Exited {
        service_id: ServiceID,
        run_id: RunId,
        exit_code: i32,
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
}

impl ProcessEvent {
    pub(crate) fn service_id(&self) -> &ServiceID {
        match self {
            Self::Killed { service_id, .. }
            | Self::Exited { service_id, .. }
            | Self::LogReaderFinished { service_id, .. }
            | Self::Healthy { service_id, .. }
            | Self::Unhealthy { service_id, .. } => service_id,
        }
    }

    pub(crate) fn run_id(&self) -> RunId {
        match self {
            Self::Killed { run_id, .. }
            | Self::Exited { run_id, .. }
            | Self::LogReaderFinished { run_id, .. }
            | Self::Healthy { run_id, .. }
            | Self::Unhealthy { run_id, .. } => *run_id,
        }
    }

    #[cfg(test)]
    pub(crate) fn to_test_event(&self) -> Event {
        match self {
            Self::Killed { service_id, .. } => Event::Killed(service_id.clone()),
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
    /// A service exited.
    Exited(ServiceID, i32),
    /// A service's PTY reader drained and exited.
    LogReaderFinished(ServiceID),
    /// A service became healthy.
    Healthy(ServiceID),
    /// A service became unhealthy.
    Unhealthy(ServiceID),
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
            Self::Exited(service_id, _) => write!(f, "Exited({service_id})"),
            Self::LogReaderFinished(service_id) => write!(f, "LogReaderFinished({service_id})"),
            Self::Healthy(service_id) => write!(f, "Healthy({service_id})"),
            Self::Unhealthy(service_id) => write!(f, "Unhealthy({service_id})"),
            Self::Disabled(service_id) => write!(f, "Disabled({service_id})"),
            Self::ClearLogs(service_id) => write!(f, "ClearLogs({service_id})"),
        }
    }
}

/// A command sent to the scheduler.
///
/// The service-control variants carry an optional [`CommandAck`]: the trusted in-process TUI passes
/// `None` (fire-and-forget), while the narrow [`super::ServiceControl`] port attaches an ack so the
/// scheduler validates and latches the generation request/response. `Command` is not `Clone`
/// because an ack is single-shot.
#[derive(Debug)]
pub enum Command {
    /// Restart a single service.
    Restart {
        /// Service to restart.
        service: ServiceID,
        /// Optional reply channel for acknowledged commands.
        ack: Option<CommandAck>,
    },
    /// Restart all enabled services.
    RestartAll {
        /// Optional reply channel for acknowledged commands.
        ack: Option<CommandAck>,
    },
    /// Disable a single service.
    Disable {
        /// Service to disable.
        service: ServiceID,
        /// Optional reply channel for acknowledged commands.
        ack: Option<CommandAck>,
    },
    /// Enable a single service.
    Enable {
        /// Service to enable.
        service: ServiceID,
        /// Optional reply channel for acknowledged commands.
        ack: Option<CommandAck>,
    },
    /// Create a dynamic service.
    StartDynamic {
        /// Definition and lease request.
        params: DynamicServiceParams,
        /// Required reply channel.
        ack: DynamicCommandAck,
    },
    /// Replace or revive a dynamic service.
    ReplaceDynamic {
        /// Existing dynamic service id.
        service: ServiceID,
        /// Required current revision.
        expected_revision: u64,
        /// Replacement definition and lease request.
        params: DynamicServiceParams,
        /// Required reply channel.
        ack: DynamicCommandAck,
    },
    /// Renew a live dynamic service's lease without restarting it.
    RenewDynamic {
        /// Existing dynamic service id.
        service: ServiceID,
        /// Required current revision.
        expected_revision: u64,
        /// Requested replacement lease.
        expires_after: Option<Lease>,
        /// Required reply channel.
        ack: DynamicCommandAck,
    },
    /// Retire a dynamic service.
    StopDynamic {
        /// Dynamic service id.
        service: ServiceID,
        /// Required reply channel.
        ack: DynamicCommandAck,
    },
    /// Send a raw input payload to a service.
    SendInput(ServiceID, Vec<u8>),
    /// Resize all PTYs.
    ResizeAll {
        /// Terminal width in columns.
        cols: u16,
        /// Terminal height in rows.
        rows: u16,
    },
}

impl Command {
    /// Fire-and-forget restart (no acknowledgement). Used by the trusted in-process TUI.
    #[must_use]
    pub fn restart(service: ServiceID) -> Self {
        Self::Restart { service, ack: None }
    }

    /// Fire-and-forget restart-all (no acknowledgement).
    #[must_use]
    pub fn restart_all() -> Self {
        Self::RestartAll { ack: None }
    }

    /// Fire-and-forget disable (no acknowledgement).
    #[must_use]
    pub fn disable(service: ServiceID) -> Self {
        Self::Disable { service, ack: None }
    }

    /// Fire-and-forget enable (no acknowledgement).
    #[must_use]
    pub fn enable(service: ServiceID) -> Self {
        Self::Enable { service, ack: None }
    }
}
