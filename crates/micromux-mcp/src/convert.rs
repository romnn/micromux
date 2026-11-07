//! Mapping control responses into tool outputs.

use micromux::{
    HealthAttempt, LogLine, LogRunSummary, ServiceCommandAck, ServiceEvent, ServiceSnapshot,
};
use micromux_control::{ErrorCode, Response};

use crate::select::ToolError;

/// Log payload returned by the control plane.
pub struct LogsResult {
    /// Returned log lines.
    pub lines: Vec<LogLine>,
    /// Whether server-side response limits dropped older content.
    pub truncated: bool,
    /// First sequence still retained by the visible stream.
    pub first_retained_seq: Option<u64>,
}

fn remote_error(code: ErrorCode, message: String) -> ToolError {
    match code {
        ErrorCode::Ambiguous => ToolError::Ambiguous(message),
        ErrorCode::Busy | ErrorCode::Timeout | ErrorCode::SchedulerStopped => {
            ToolError::Busy(format!("{code:?}: {message}"))
        }
        ErrorCode::InvalidState => ToolError::InvalidState(message),
        ErrorCode::ConfigReload => ToolError::ConfigReload(message),
        ErrorCode::UnsupportedPlatform => ToolError::Unsupported,
        ErrorCode::ProtocolVersionMismatch | ErrorCode::Internal => {
            ToolError::Unexpected(format!("{code:?}: {message}"))
        }
        ErrorCode::UnknownService
        | ErrorCode::UnknownRun
        | ErrorCode::NoSession
        | ErrorCode::PolicyDenied
        | ErrorCode::LimitExceeded
        | ErrorCode::RevisionMismatch
        | ErrorCode::InvalidSpec
        | ErrorCode::BadRequest
        | ErrorCode::Unknown => ToolError::Remote { code, message },
    }
}

/// Extract the service list, mapping a remote error into a typed [`ToolError`].
///
/// # Errors
///
/// Returns a [`ToolError`] if the session replied with an error or an unexpected response.
pub fn services(response: Response) -> Result<Vec<ServiceSnapshot>, ToolError> {
    match response {
        Response::Services(services) => Ok(services),
        Response::Error { code, message } => Err(remote_error(code, message)),
        other => Err(ToolError::Unexpected(format!("{other:?}"))),
    }
}

/// Extract one service snapshot, mapping a remote error into a typed [`ToolError`].
///
/// # Errors
///
/// Returns a [`ToolError`] if the session replied with an error or an unexpected response.
pub fn service(response: Response) -> Result<ServiceSnapshot, ToolError> {
    match response {
        Response::Service(service) => Ok(*service),
        Response::Error { code, message } => Err(remote_error(code, message)),
        other => Err(ToolError::Unexpected(format!("{other:?}"))),
    }
}

/// Extract log lines.
///
/// # Errors
///
/// Returns a [`ToolError`] if the session replied with an error or an unexpected response.
pub fn logs(response: Response) -> Result<LogsResult, ToolError> {
    match response {
        Response::Logs {
            lines,
            truncated,
            first_retained_seq,
        } => Ok(LogsResult {
            lines,
            truncated,
            first_retained_seq,
        }),
        Response::Error { code, message } => Err(remote_error(code, message)),
        other => Err(ToolError::Unexpected(format!("{other:?}"))),
    }
}

/// Extract retained log run summaries.
///
/// # Errors
///
/// Returns a [`ToolError`] if the session replied with an error or an unexpected response.
pub fn log_runs(response: Response) -> Result<Vec<LogRunSummary>, ToolError> {
    match response {
        Response::LogRuns { runs } => Ok(runs),
        Response::Error { code, message } => Err(remote_error(code, message)),
        other => Err(ToolError::Unexpected(format!("{other:?}"))),
    }
}

/// Extract the latest healthcheck attempt.
///
/// # Errors
///
/// Returns a [`ToolError`] if the session replied with an error or an unexpected response.
pub fn health(response: Response) -> Result<Option<HealthAttempt>, ToolError> {
    match response {
        Response::Health(attempt) => Ok(attempt),
        Response::Error { code, message } => Err(remote_error(code, message)),
        other => Err(ToolError::Unexpected(format!("{other:?}"))),
    }
}

/// Extract the retained healthcheck attempt history.
///
/// # Errors
///
/// Returns a [`ToolError`] if the session replied with an error or an unexpected response.
pub fn health_history(response: Response) -> Result<Vec<HealthAttempt>, ToolError> {
    match response {
        Response::HealthHistory { attempts } => Ok(attempts),
        Response::Error { code, message } => Err(remote_error(code, message)),
        other => Err(ToolError::Unexpected(format!("{other:?}"))),
    }
}

/// Extract a page of lifecycle events and its truncation marker.
///
/// # Errors
///
/// Returns a [`ToolError`] if the session replied with an error or an unexpected response.
pub fn events(response: Response) -> Result<(Vec<ServiceEvent>, bool), ToolError> {
    match response {
        Response::Events { events, truncated } => Ok((events, truncated)),
        Response::Error { code, message } => Err(remote_error(code, message)),
        other => Err(ToolError::Unexpected(format!("{other:?}"))),
    }
}

/// Extract the acknowledgements of a mutation.
///
/// # Errors
///
/// Returns a [`ToolError`] if the session replied with an error or an unexpected response.
pub fn accepted(response: Response) -> Result<Vec<ServiceCommandAck>, ToolError> {
    match response {
        Response::Accepted { services } => Ok(services),
        Response::Error { code, message } => Err(remote_error(code, message)),
        other => Err(ToolError::Unexpected(format!("{other:?}"))),
    }
}

/// Extract a dynamic-service mutation receipt.
///
/// # Errors
///
/// Returns a [`ToolError`] if the session replied with an error or an unexpected response.
pub fn dynamic_service(
    response: Response,
) -> Result<micromux_control::DynamicServiceAck, ToolError> {
    match response {
        Response::DynamicService(receipt) => Ok(receipt),
        Response::Error { code, message } => Err(remote_error(code, message)),
        other => Err(ToolError::Unexpected(format!("{other:?}"))),
    }
}

/// Extract a config-reconciliation receipt.
///
/// # Errors
///
/// Returns a [`ToolError`] if the session replied with an error or an unexpected response.
pub fn reconcile(response: Response) -> Result<micromux::ReconcileReceipt, ToolError> {
    match response {
        Response::Reconcile(receipt) => Ok(receipt),
        Response::Error { code, message } => Err(remote_error(code, message)),
        other => Err(ToolError::Unexpected(format!("{other:?}"))),
    }
}

/// Extract an environment with its values.
///
/// # Errors
///
/// Returns a [`ToolError`] if the session replied with an error or an unexpected response.
pub fn environment(response: Response) -> Result<micromux::ResolvedEnvironment, ToolError> {
    match response {
        Response::Environment(environment) => Ok(environment),
        Response::Error { code, message } => Err(remote_error(code, message)),
        other => Err(ToolError::Unexpected(format!("{other:?}"))),
    }
}

/// Confirm a shutdown was acknowledged.
///
/// # Errors
///
/// Returns a [`ToolError`] if the session replied with an error or an unexpected response.
pub fn shutting_down(response: Response) -> Result<(), ToolError> {
    match response {
        Response::ShuttingDown => Ok(()),
        Response::Error { code, message } => Err(remote_error(code, message)),
        other => Err(ToolError::Unexpected(format!("{other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_error_maps_retryable_errors_to_busy() {
        let err = remote_error(ErrorCode::Timeout, "scheduler did not respond".to_string());

        assert!(matches!(err, ToolError::Busy(message) if message.contains("Timeout")));
    }

    #[test]
    fn remote_error_maps_config_reload_separately_from_state_errors() {
        let err = remote_error(ErrorCode::ConfigReload, "bad yaml".to_string());

        assert!(matches!(err, ToolError::ConfigReload(message) if message == "bad yaml"));
    }

    #[test]
    fn remote_error_keeps_request_errors_as_remote() {
        let err = remote_error(ErrorCode::UnknownRun, "run gone".to_string());

        assert!(
            matches!(err, ToolError::Remote { code: ErrorCode::UnknownRun, message } if message == "run gone")
        );
    }

    #[test]
    fn remote_error_maps_protocol_failures_to_internal_errors() {
        let err = remote_error(
            ErrorCode::ProtocolVersionMismatch,
            "session speaks v1".to_string(),
        );

        assert!(
            matches!(err, ToolError::Unexpected(message) if message.contains("ProtocolVersionMismatch"))
        );
    }
}
