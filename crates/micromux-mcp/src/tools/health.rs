use micromux::{Execution, Health, HealthAttempt, ServiceSnapshot};
use micromux_control::{ErrorCode, Request};

use crate::select::ToolError;
use crate::{DIAGNOSE_LOG_SCAN, SessionConn, convert, logproc};

pub(crate) fn health_attempt_matches_snapshot(
    snapshot: &ServiceSnapshot,
    attempt: &HealthAttempt,
) -> bool {
    snapshot.execution == Execution::Running && attempt.run_generation == snapshot.run_generation
}

/// Trim a healthcheck attempt's captured output to its last lines, so a chatty probe doesn't bloat
/// the timeout report.
pub(crate) fn bounded_attempt(mut attempt: HealthAttempt) -> HealthAttempt {
    const MAX_OUTPUT_LINES: usize = 20;
    if attempt.output.len() > MAX_OUTPUT_LINES {
        let drop = attempt.output.len() - MAX_OUTPUT_LINES;
        attempt.output.drain(0..drop);
    }
    attempt
}

pub(crate) fn service_needs_diagnosis(snapshot: &ServiceSnapshot) -> bool {
    if snapshot.desired == micromux::Desired::Disabled {
        return false;
    }
    snapshot.execution != Execution::Running
        || (snapshot.healthcheck_configured && snapshot.health != Some(Health::Healthy))
}

pub(crate) fn is_likely_cause_entry(entry: &logproc::ProcessedEntry) -> bool {
    if matches!(entry.level, Some("error" | "fatal")) {
        return true;
    }
    let lower = entry.line.to_ascii_lowercase();
    lower.contains("[stderr]")
        || lower.contains("error")
        || lower.contains("failed")
        || lower.contains("panic")
        || lower.contains("exception")
        || lower.contains("traceback")
        || lower.contains("does not exist")
}

pub(crate) fn diagnosis_hint(
    snapshot: &ServiceSnapshot,
    latest_healthcheck: Option<&HealthAttempt>,
    error_log_tail: &[logproc::ProcessedEntry],
) -> String {
    match snapshot.execution {
        Execution::Exited => {
            if error_log_tail.is_empty() {
                "service exited; inspect get_logs for the full retained run".to_string()
            } else {
                "service exited; error_log_tail contains likely cause lines".to_string()
            }
        }
        Execution::Running if snapshot.healthcheck_configured => {
            if latest_healthcheck.is_some() {
                "service is running but healthcheck is not healthy; inspect latest_healthcheck output"
                    .to_string()
            } else {
                "service is running but healthcheck is not healthy yet; no healthcheck attempt has completed"
                    .to_string()
            }
        }
        Execution::Pending | Execution::Starting => {
            "service has not finished starting; inspect recent logs".to_string()
        }
        Execution::Stopping => "service is stopping".to_string(),
        Execution::Unknown => "service reported an unknown execution state".to_string(),
        Execution::Running => {
            "service state needs attention; inspect snapshot and logs".to_string()
        }
    }
}

/// A factual next-step hint for a `wait_for_healthy` timeout, derived from the execution sub-state.
pub(crate) fn timeout_hint(snapshot: &ServiceSnapshot) -> &'static str {
    match snapshot.execution {
        Execution::Running => {
            if snapshot.healthcheck_configured && snapshot.health != Some(Health::Healthy) {
                "the process is running but its healthcheck has not passed yet — inspect \
                 latest_healthcheck (or call get_health); if the command is still compiling/starting, \
                 wait again with a longer timeout_secs"
            } else {
                "the process is running and may still be completing startup — wait again or inspect \
                 get_logs"
            }
        }
        Execution::Pending | Execution::Starting => {
            "the process has not finished starting — wait again with a longer timeout_secs or inspect \
             get_logs"
        }
        Execution::Stopping => "the service is stopping",
        Execution::Unknown => {
            "the session reported an unknown service state — inspect list_services"
        }
        Execution::Exited => {
            "the run has exited — inspect get_logs and the service's last_exit_code"
        }
    }
}

/// Best-effort fetch of the current live run's latest healthcheck attempt, used only to enrich a
/// timeout or diagnosis report. Any error degrades to `None` — the report is still useful without
/// it.
async fn latest_health(conn: &mut SessionConn, service: &str) -> Option<HealthAttempt> {
    let response = conn
        .request(Request::GetHealth {
            service: service.to_string(),
        })
        .await
        .ok()?;
    convert::health(response).ok().flatten()
}

pub(crate) async fn latest_health_for_snapshot(
    conn: &mut SessionConn,
    snapshot: &ServiceSnapshot,
) -> Option<HealthAttempt> {
    if snapshot.execution != Execution::Running {
        return None;
    }
    let attempt = latest_health(conn, &snapshot.id).await?;
    // Diagnose/wait enrichment reads health in a second request; a restart between the snapshot and
    // health requests must not attach the next run's probe output to the previous run's state.
    health_attempt_matches_snapshot(snapshot, &attempt).then_some(attempt)
}

pub(crate) async fn likely_cause_log_tail(
    conn: &mut SessionConn,
    snapshot: &ServiceSnapshot,
    limit: usize,
) -> Result<(Vec<logproc::ProcessedEntry>, bool), ToolError> {
    let logs = logs_for_diagnosis(conn, snapshot).await?;
    let mut entries = logproc::shape(
        &logs.lines,
        &logproc::Shape {
            format: logproc::LogFormat::Compact,
            ..logproc::Shape::default()
        },
    )
    .into_iter()
    .filter(is_likely_cause_entry)
    .collect::<Vec<_>>();
    let mut truncated = logs.truncated;
    if entries.len() > limit {
        truncated |= logproc::tail_preserving_record_boundaries(&mut entries, limit);
    }
    Ok((entries, truncated))
}

async fn logs_for_diagnosis(
    conn: &mut SessionConn,
    snapshot: &ServiceSnapshot,
) -> Result<convert::LogsResult, ToolError> {
    if snapshot.run_generation > 0 {
        match fetch_log_tail(conn, &snapshot.id, Some(snapshot.run_generation)).await {
            Ok(logs) => return Ok(logs),
            Err(ToolError::Remote {
                code: ErrorCode::UnknownRun,
                ..
            }) => {}
            Err(err) => return Err(err),
        }
    }
    fetch_log_tail(conn, &snapshot.id, None).await
}

async fn fetch_log_tail(
    conn: &mut SessionConn,
    service: &str,
    run_generation: Option<u64>,
) -> Result<convert::LogsResult, ToolError> {
    let response = conn
        .request(Request::GetLogs {
            service: service.to_string(),
            run_generation,
            tail: Some(DIAGNOSE_LOG_SCAN),
        })
        .await?;
    convert::logs(response)
}
