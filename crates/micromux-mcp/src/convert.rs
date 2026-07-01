//! Mapping control responses into tool outputs, and the `wait_for_healthy` evaluation.

use micromux::{
    Desired, Execution, Health, HealthAttempt, LogLine, LogRunSummary, ServiceCommandAck,
    ServiceSnapshot,
};
use micromux_control::{ErrorCode, Response};

use crate::select::ToolError;

/// Log payload returned by the control plane.
pub struct LogsResult {
    /// Returned log lines.
    pub lines: Vec<LogLine>,
    /// Whether server-side response limits dropped older content.
    pub truncated: bool,
}

fn remote_error(code: ErrorCode, message: String) -> ToolError {
    match code {
        ErrorCode::Ambiguous => ToolError::Ambiguous(message),
        ErrorCode::Busy | ErrorCode::Timeout | ErrorCode::SchedulerStopped => {
            ToolError::Busy(format!("{code:?}: {message}"))
        }
        ErrorCode::InvalidState => ToolError::InvalidState(message),
        ErrorCode::UnsupportedPlatform => ToolError::Unsupported,
        ErrorCode::ProtocolVersionMismatch | ErrorCode::Internal => {
            ToolError::Unexpected(format!("{code:?}: {message}"))
        }
        ErrorCode::UnknownService
        | ErrorCode::UnknownRun
        | ErrorCode::NoSession
        | ErrorCode::BadRequest => ToolError::Remote { code, message },
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

/// Extract log lines.
///
/// # Errors
///
/// Returns a [`ToolError`] if the session replied with an error or an unexpected response.
pub fn logs(response: Response) -> Result<LogsResult, ToolError> {
    match response {
        Response::Logs { lines, truncated } => Ok(LogsResult { lines, truncated }),
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

/// The verdict of one `wait_for_healthy` evaluation against a fresh snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// Keep waiting.
    Pending,
    /// The target run is up (and healthy, if a healthcheck is configured).
    Healthy,
    /// The target run exited; carries its exit code.
    Exited(Option<i32>),
    /// The service is disabled and will never become healthy.
    InvalidState,
}

/// Evaluate the generation-aware healthy condition against a snapshot.
///
/// With `after = Some(G)` we resolve only on a run with `run_generation > G`, so a restart's caller
/// never observes the pre-restart state. With `after = None` we accept the current run.
#[must_use]
pub fn evaluate(snapshot: &ServiceSnapshot, after: Option<u64>) -> WaitOutcome {
    if snapshot.desired == Desired::Disabled {
        // A disabled service will never become healthy — fail fast rather than time out.
        return WaitOutcome::InvalidState;
    }

    let generation_ready = match after {
        Some(generation) => snapshot.run_generation > generation,
        // `run_generation == 0` means never started: wait for the first run to come up.
        None => snapshot.run_generation >= 1,
    };
    if !generation_ready {
        return WaitOutcome::Pending;
    }

    match snapshot.execution {
        Execution::Exited => WaitOutcome::Exited(snapshot.last_exit_code),
        Execution::Running => {
            if !snapshot.healthcheck_configured || snapshot.health == Some(Health::Healthy) {
                WaitOutcome::Healthy
            } else {
                WaitOutcome::Pending
            }
        }
        Execution::Pending | Execution::Starting | Execution::Stopping => WaitOutcome::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use micromux::RestartPolicy;
    use similar_asserts::assert_eq;

    fn snapshot() -> ServiceSnapshot {
        let mut snapshot = ServiceSnapshot::initial(
            "svc".to_string(),
            "svc".to_string(),
            Vec::new(),
            false,
            RestartPolicy::Never,
        );
        snapshot.execution = Execution::Running;
        snapshot.run_generation = 1;
        snapshot
    }

    #[test]
    fn waits_for_a_new_generation_after_restart() {
        let mut snap = snapshot();
        snap.run_generation = 1;
        // Restart returned G=1; the pre-restart Running state must not satisfy the wait.
        assert_eq!(evaluate(&snap, Some(1)), WaitOutcome::Pending);
        snap.run_generation = 2;
        assert_eq!(evaluate(&snap, Some(1)), WaitOutcome::Healthy);
    }

    #[test]
    fn requires_healthy_when_a_healthcheck_is_configured() {
        let mut snap = snapshot();
        snap.healthcheck_configured = true;
        snap.health = None;
        assert_eq!(evaluate(&snap, None), WaitOutcome::Pending);
        snap.health = Some(Health::Unhealthy);
        assert_eq!(evaluate(&snap, None), WaitOutcome::Pending);
        snap.health = Some(Health::Healthy);
        assert_eq!(evaluate(&snap, None), WaitOutcome::Healthy);
    }

    #[test]
    fn disabled_fails_fast() {
        let mut snap = snapshot();
        snap.desired = Desired::Disabled;
        assert_eq!(evaluate(&snap, None), WaitOutcome::InvalidState);
    }

    #[test]
    fn exited_reports_exit_code() {
        let mut snap = snapshot();
        snap.execution = Execution::Exited;
        snap.last_exit_code = Some(3);
        assert_eq!(evaluate(&snap, None), WaitOutcome::Exited(Some(3)));
    }

    #[test]
    fn never_started_waits_for_first_run() {
        let mut snap = snapshot();
        snap.run_generation = 0;
        snap.execution = Execution::Pending;
        assert_eq!(evaluate(&snap, None), WaitOutcome::Pending);
    }

    #[test]
    fn new_generation_that_exited_reports_the_exit_code() {
        // restart returned G=1; the new run (gen 2) crashed — the wait must surface the exit, not
        // keep waiting.
        let mut snap = snapshot();
        snap.run_generation = 2;
        snap.execution = Execution::Exited;
        snap.last_exit_code = Some(1);
        assert_eq!(evaluate(&snap, Some(1)), WaitOutcome::Exited(Some(1)));
    }

    #[test]
    fn disabled_takes_precedence_over_the_generation_gate() {
        // Even when waiting for a future generation, a disabled service fails fast (it will never
        // become healthy) rather than blocking until timeout.
        let mut snap = snapshot();
        snap.desired = Desired::Disabled;
        snap.run_generation = 1;
        assert_eq!(evaluate(&snap, Some(5)), WaitOutcome::InvalidState);
    }

    #[test]
    fn starting_and_stopping_are_pending() {
        let mut snap = snapshot();
        snap.execution = Execution::Starting;
        assert_eq!(evaluate(&snap, None), WaitOutcome::Pending);
        snap.execution = Execution::Stopping;
        assert_eq!(evaluate(&snap, None), WaitOutcome::Pending);
    }

    #[test]
    fn remote_error_maps_retryable_errors_to_busy() {
        let err = remote_error(ErrorCode::Timeout, "scheduler did not respond".to_string());

        assert!(matches!(err, ToolError::Busy(message) if message.contains("Timeout")));
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
