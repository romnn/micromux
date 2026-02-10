//! Scheduler event and command types.
//!
//! This module defines the types used for communication between the scheduler, services, and UI.
//! The main types are:
//! - [`Event`]: a one-way notification emitted by the scheduler.
//! - [`Command`]: a request issued to the scheduler.
//! - [`State`]: the current lifecycle state of a service.

use crate::health_check::Health;

/// Unique identifier for a service.
pub type ServiceID = String;

/// The lifecycle state of a service.
#[derive(Debug)]
pub enum State {
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

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "Pending"),
            Self::Starting => write!(f, "Starting"),
            Self::Running { health: None } => write!(f, "Running"),
            Self::Running {
                health: Some(health),
            } => write!(f, "Running({health})"),
            Self::Disabled => write!(f, "Disabled"),
            Self::Exited { exit_code } => write!(f, "Exited({exit_code})"),
            Self::Killed => write!(f, "Killed"),
        }
    }
}

/// A scheduler event.
///
/// Events are emitted as the scheduler observes state changes or receives output from managed
/// services.
#[derive(Debug)]
pub enum Event {
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
        /// How this line should update the UI buffer.
        update: LogUpdateKind,
        /// The raw (possibly ANSI-colored) line.
        line: String,
    },
    /// A healthcheck attempt is about to start.
    HealthCheckStarted {
        /// Service this healthcheck belongs to.
        service_id: ServiceID,
        /// Monotonic attempt number.
        attempt: u64,
        /// The command that is executed for this healthcheck.
        command: String,
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
pub enum LogUpdateKind {
    /// Append a new line to the log buffer.
    Append,
    /// Replace the most recent line in the log buffer.
    ReplaceLast,
}

/// Origin stream of output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

impl Event {
    /// Returns the [`ServiceID`] associated with this event.
    #[must_use]
    pub fn service_id(&self) -> &ServiceID {
        match self {
            Self::Started { service_id }
            | Self::LogLine { service_id, .. }
            | Self::HealthCheckStarted { service_id, .. }
            | Self::HealthCheckLogLine { service_id, .. }
            | Self::HealthCheckFinished { service_id, .. }
            | Self::Killed(service_id)
            | Self::Exited(service_id, _)
            | Self::Healthy(service_id)
            | Self::Unhealthy(service_id)
            | Self::Disabled(service_id)
            | Self::ClearLogs(service_id) => service_id,
        }
    }
}

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
#[derive(Debug, Clone)]
pub enum Command {
    /// Restart a single service.
    Restart(ServiceID),
    /// Restart all services.
    RestartAll,
    /// Disable a single service.
    Disable(ServiceID),
    /// Enable a single service.
    Enable(ServiceID),
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
