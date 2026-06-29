//! Scheduler command and lifecycle types.

use super::control::CommandAck;
use crate::health_check::Health;

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
    LogLine {
        service_id: ServiceID,
        run_id: RunId,
        stream: OutputStream,
        update: LogUpdateKind,
        line: String,
    },
    HealthCheckStarted {
        service_id: ServiceID,
        run_id: RunId,
        attempt: u64,
        command: String,
    },
    HealthCheckLogLine {
        service_id: ServiceID,
        run_id: RunId,
        attempt: u64,
        stream: OutputStream,
        line: String,
    },
    HealthCheckFinished {
        service_id: ServiceID,
        run_id: RunId,
        attempt: u64,
        success: bool,
        exit_code: i32,
    },
    Killed {
        service_id: ServiceID,
        run_id: RunId,
    },
    Exited {
        service_id: ServiceID,
        run_id: RunId,
        exit_code: i32,
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
            Self::LogLine { service_id, .. }
            | Self::HealthCheckStarted { service_id, .. }
            | Self::HealthCheckLogLine { service_id, .. }
            | Self::HealthCheckFinished { service_id, .. }
            | Self::Killed { service_id, .. }
            | Self::Exited { service_id, .. }
            | Self::Healthy { service_id, .. }
            | Self::Unhealthy { service_id, .. } => service_id,
        }
    }

    pub(crate) fn run_id(&self) -> RunId {
        match self {
            Self::LogLine { run_id, .. }
            | Self::HealthCheckStarted { run_id, .. }
            | Self::HealthCheckLogLine { run_id, .. }
            | Self::HealthCheckFinished { run_id, .. }
            | Self::Killed { run_id, .. }
            | Self::Exited { run_id, .. }
            | Self::Healthy { run_id, .. }
            | Self::Unhealthy { run_id, .. } => *run_id,
        }
    }

    #[cfg(test)]
    pub(crate) fn to_test_event(&self) -> Event {
        match self {
            Self::LogLine {
                service_id,
                stream,
                line,
                ..
            } => Event::LogLine {
                service_id: service_id.clone(),
                stream: *stream,
                line: line.clone(),
            },
            Self::HealthCheckStarted {
                service_id,
                attempt,
                ..
            } => Event::HealthCheckStarted {
                service_id: service_id.clone(),
                attempt: *attempt,
            },
            Self::HealthCheckLogLine {
                service_id,
                attempt,
                stream,
                line,
                ..
            } => Event::HealthCheckLogLine {
                service_id: service_id.clone(),
                attempt: *attempt,
                stream: *stream,
                line: line.clone(),
            },
            Self::HealthCheckFinished {
                service_id,
                attempt,
                success,
                exit_code,
                ..
            } => Event::HealthCheckFinished {
                service_id: service_id.clone(),
                attempt: *attempt,
                success: *success,
                exit_code: *exit_code,
            },
            Self::Killed { service_id, .. } => Event::Killed(service_id.clone()),
            Self::Exited {
                service_id,
                exit_code,
                ..
            } => Event::Exited(service_id.clone(), *exit_code),
            Self::Healthy { service_id, .. } => Event::Healthy(service_id.clone()),
            Self::Unhealthy { service_id, .. } => Event::Unhealthy(service_id.clone()),
        }
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) enum Event {
    /// A service has started.
    Started {
        /// Service that started.
        service_id: ServiceID,
    },
    /// A new log line was produced.
    LogLine {
        /// Service that produced this output.
        service_id: ServiceID,
        /// Which stream produced the output.
        stream: OutputStream,
        /// The raw (possibly ANSI-colored) line.
        line: String,
    },
    /// A healthcheck attempt is about to start.
    HealthCheckStarted {
        /// Service this healthcheck belongs to.
        service_id: ServiceID,
        /// Monotonic attempt number.
        attempt: u64,
    },
    /// A healthcheck produced a log line.
    HealthCheckLogLine {
        /// Service this healthcheck belongs to.
        service_id: ServiceID,
        /// Monotonic attempt number.
        attempt: u64,
        /// Which stream produced the output.
        stream: OutputStream,
        /// The output line.
        line: String,
    },
    /// A healthcheck finished.
    HealthCheckFinished {
        /// Service this healthcheck belongs to.
        service_id: ServiceID,
        /// Monotonic attempt number.
        attempt: u64,
        /// Whether the healthcheck exited successfully.
        success: bool,
        /// Exit code of the healthcheck process.
        exit_code: i32,
    },
    /// A service was killed.
    Killed(ServiceID),
    /// A service exited.
    Exited(ServiceID, i32),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

#[cfg(test)]
impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Started { service_id, .. } => write!(f, "Started({service_id})"),
            Self::LogLine {
                service_id, stream, ..
            } => write!(f, "LogLine({service_id}, {stream:?})"),
            Self::HealthCheckStarted {
                service_id,
                attempt,
                ..
            } => write!(f, "HealthCheckStarted({service_id}, attempt={attempt})"),
            Self::HealthCheckLogLine {
                service_id,
                stream,
                attempt,
                ..
            } => write!(
                f,
                "HealthCheckLogLine({service_id}, {stream:?}, attempt={attempt})"
            ),
            Self::HealthCheckFinished {
                service_id,
                attempt,
                success,
                exit_code,
            } => write!(
                f,
                "HealthCheckFinished({service_id}, attempt={attempt}, success={success}, exit_code={exit_code})"
            ),
            Self::Killed(service_id) => write!(f, "Killed({service_id})"),
            Self::Exited(service_id, _) => write!(f, "Exited({service_id})"),
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
