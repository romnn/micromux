use crate::health_check::Health;

pub type ServiceID = String;

#[derive(Debug, enum_as_inner::EnumAsInner)]
pub enum State {
    /// Service has not yet started.
    Pending,
    Starting,
    /// Service is running.
    Running { health: Option<Health> },
    /// Service is disabled.
    Disabled,
    /// Service exited with code.
    Exited { exit_code: i32 },
    /// Service has been killed and is awaiting exit
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

#[derive(Debug)]
pub enum Event {
    Started { service_id: ServiceID },
    LogLine {
        service_id: ServiceID,
        stream: OutputStream,
        update: LogUpdateKind,
        line: String,
    },
    HealthCheckStarted {
        service_id: ServiceID,
        attempt: u64,
        command: String,
    },
    HealthCheckLogLine {
        service_id: ServiceID,
        attempt: u64,
        stream: OutputStream,
        line: String,
    },
    HealthCheckFinished {
        service_id: ServiceID,
        attempt: u64,
        success: bool,
        exit_code: i32,
    },
    Killed(ServiceID),
    Exited(ServiceID, i32),
    Healthy(ServiceID),
    Unhealthy(ServiceID),
    Disabled(ServiceID),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogUpdateKind {
    Append,
    ReplaceLast,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

impl Event {
    pub fn service_id(&self) -> &ServiceID {
        match self {
            Self::Started { service_id, .. } => service_id,
            Self::LogLine { service_id, .. } => service_id,
            Self::HealthCheckStarted { service_id, .. } => service_id,
            Self::HealthCheckLogLine { service_id, .. } => service_id,
            Self::HealthCheckFinished { service_id, .. } => service_id,
            Self::Killed(service_id) => service_id,
            Self::Exited(service_id, _) => service_id,
            Self::Healthy(service_id) => service_id,
            Self::Unhealthy(service_id) => service_id,
            Self::Disabled(service_id) => service_id,
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
        }
    }
}

#[derive(Debug, Clone)]
pub enum Command {
    Restart(ServiceID),
    RestartAll,
    Disable(ServiceID),
    Enable(ServiceID),
    SendInput(ServiceID, Vec<u8>),
    ResizeAll { cols: u16, rows: u16 },
}
