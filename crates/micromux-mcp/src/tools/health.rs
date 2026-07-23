use std::fmt::Write as _;
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::Duration;

use micromux::{ChangeKind, Execution, Health, HealthAttempt, ServiceEvent, ServiceSnapshot};
use micromux_control::{Client, ControlEndpoint, ErrorCode, Request};
use schemars::JsonSchema;
use serde::Serialize;

use crate::select::ToolError;
use crate::tools::portowner;
use crate::{DIAGNOSE_LOG_SCAN, SessionConn, WAIT_POLL_FLOOR, convert, logproc, select};

const PORT_PROBE_BUDGET: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Signal {
    pub(crate) kind: SignalKind,
    pub(crate) detail: String,
    pub(crate) next_probe: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[non_exhaustive]
#[serde(rename_all = "snake_case")]
pub(crate) enum SignalKind {
    ExitSignal,
    CrashLoop,
    HealthcheckTimeout,
    PortUnavailable,
}

#[derive(Serialize, JsonSchema)]
pub(crate) struct ServiceDiagnosis {
    pub(crate) service: String,
    pub(crate) snapshot: ServiceSnapshot,
    pub(crate) latest_healthcheck: Option<HealthAttempt>,
    pub(crate) timeline: Vec<ServiceEvent>,
    pub(crate) signals: Vec<Signal>,
    pub(crate) error_log_tail: Vec<logproc::ProcessedEntry>,
    pub(crate) logs_truncated: bool,
    pub(crate) hint: String,
}

pub(crate) enum WaitConclusion {
    Healthy(ServiceSnapshot),
    Exited(ServiceSnapshot),
    Timeout {
        snapshot: ServiceSnapshot,
        latest_healthcheck: Option<HealthAttempt>,
    },
}

enum HealthVerdict {
    Healthy,
    Exited,
    InvalidState,
}

impl WaitConclusion {
    pub(crate) fn snapshot(&self) -> &ServiceSnapshot {
        match self {
            Self::Healthy(snapshot) | Self::Exited(snapshot) | Self::Timeout { snapshot, .. } => {
                snapshot
            }
        }
    }

    pub(crate) fn needs_diagnosis(&self) -> bool {
        matches!(self, Self::Exited(_) | Self::Timeout { .. })
    }
}

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

fn exit_signal(snapshot: &ServiceSnapshot) -> Option<Signal> {
    if snapshot.execution != Execution::Exited {
        return None;
    }
    let exit_code = snapshot.last_exit_code?;
    let meaning = match exit_code {
        137 => " (SIGKILL; often OOM)",
        143 => " (SIGTERM)",
        139 => " (SIGSEGV)",
        127 => " (command not found)",
        126 => " (command found but not executable)",
        0 => " (clean exit)",
        _ => "",
    };
    Some(Signal {
        kind: SignalKind::ExitSignal,
        detail: format!("last run exited with status {exit_code}{meaning}"),
        next_probe: "inspect error_log_tail and the full retained run with get_logs",
    })
}

fn crash_loop_signal(snapshot: &ServiceSnapshot) -> Option<Signal> {
    let restart = snapshot.restart_state.as_ref()?;
    let remaining = restart.restarts_remaining.map_or_else(
        || "unbounded".to_string(),
        |remaining| remaining.to_string(),
    );
    Some(Signal {
        kind: SignalKind::CrashLoop,
        detail: format!(
            "automatic restart backoff is active for {:?}; remaining restart budget: {remaining}",
            restart.backoff_delay
        ),
        next_probe: "inspect the exit signal and retained logs before the next restart attempt",
    })
}

fn healthcheck_signal(
    snapshot: &ServiceSnapshot,
    latest_healthcheck: Option<&HealthAttempt>,
) -> Option<Signal> {
    if !snapshot.healthcheck_configured
        || snapshot.execution != Execution::Running
        || snapshot.health == Some(Health::Healthy)
    {
        return None;
    }

    let timing = snapshot.healthcheck.as_ref().map_or_else(
        || "timing unavailable from this session snapshot".to_string(),
        |config| {
            format!(
                "retries: {}, interval: {:?}, timeout: {:?}",
                config.retries, config.interval, config.timeout
            )
        },
    );
    let latest = match latest_healthcheck.and_then(|attempt| attempt.result) {
        Some(result) if result.success => {
            format!(
                "latest completed attempt succeeded with exit status {}",
                result.exit_code
            )
        }
        Some(result) => {
            format!(
                "latest completed attempt failed with exit status {}",
                result.exit_code
            )
        }
        None => "no healthcheck attempt has completed yet".to_string(),
    };
    Some(Signal {
        kind: SignalKind::HealthcheckTimeout,
        detail: format!("healthcheck has not become healthy ({timing}); {latest}"),
        next_probe: "inspect latest_healthcheck output and wait for another state change",
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PortProbe {
    Available,
    Held,
    Unknown,
}

fn classify_bind_result(result: io::Result<tokio::net::TcpListener>) -> PortProbe {
    match result {
        Ok(listener) => {
            drop(listener);
            PortProbe::Available
        }
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => PortProbe::Held,
        Err(_) => PortProbe::Unknown,
    }
}

async fn probe_port(port: u16) -> PortProbe {
    let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    match tokio::time::timeout(PORT_PROBE_BUDGET, tokio::net::TcpListener::bind(address)).await {
        Ok(result) => classify_bind_result(result),
        Err(_) => PortProbe::Unknown,
    }
}

async fn port_signals(snapshot: &ServiceSnapshot) -> Vec<Signal> {
    // Probe only when micromux holds no live process for this service: a Running service has
    // plausibly bound its own port, a Stopping one is still draining and may hold it too, and
    // Unknown (a newer peer's state) gives no basis to attribute the holder. A held port while
    // Pending/Starting/Exited is the useful fact — a foreign process or an orphaned child of a
    // previous run will make the next bind fail.
    if !matches!(
        snapshot.execution,
        Execution::Pending | Execution::Starting | Execution::Exited
    ) {
        return Vec::new();
    }
    let mut signals = Vec::new();
    for port in &snapshot.advertised_ports {
        if probe_port(*port).await == PortProbe::Held {
            let mut detail = format!(
                "advertised port {port} is already held on 127.0.0.1 while the service is {:?}",
                snapshot.execution
            );
            let micromux_owner = micromux_port_owner(*port).await;
            if let Some(owner) = &micromux_owner {
                let _ = write!(
                    detail,
                    "; held by service '{}' of session '{}' ({})",
                    owner.service, owner.session, owner.config_path
                );
            }
            let process_owner = tokio::task::spawn_blocking({
                let port = *port;
                move || portowner::listening_owner(port)
            })
            .await
            .ok()
            .flatten();
            match (micromux_owner, process_owner) {
                (_, Some(owner)) => {
                    let _ = write!(
                        detail,
                        "; listener process pid {}: {}",
                        owner.pid, owner.command
                    );
                }
                // Never assert "another user" when the micromux cross-reference already named
                // the (same-user) owner and only the OS-level lookup came up empty.
                (None, None) => {
                    detail.push_str("; held by a process owned by another user or unknown");
                }
                (Some(_), None) => {}
            }
            signals.push(Signal {
                kind: SignalKind::PortUnavailable,
                detail,
                next_probe: "confirm before freeing the port — micromux never kills it for you",
            });
        }
    }
    signals
}

struct MicromuxPortOwner {
    service: String,
    session: String,
    config_path: String,
}

async fn micromux_port_owner(port: u16) -> Option<MicromuxPortOwner> {
    for resolved in select::answering_sessions().await.ok()? {
        let Ok(mut client) = Client::connect(&resolved.endpoint).await else {
            continue;
        };
        let Ok(response) = client.request(Request::ListServices).await else {
            continue;
        };
        let Ok(services) = convert::services(response) else {
            continue;
        };
        if let Some(service) = services.into_iter().find(|service| {
            matches!(service.execution, Execution::Running | Execution::Stopping)
                && service.advertised_ports.contains(&port)
        }) {
            return Some(MicromuxPortOwner {
                service: service.id,
                session: resolved.info.name,
                config_path: resolved.info.config_path,
            });
        }
    }
    None
}

pub(crate) async fn supervisor_signals(
    snapshot: &ServiceSnapshot,
    latest_healthcheck: Option<&HealthAttempt>,
) -> Vec<Signal> {
    let mut signals = [
        exit_signal(snapshot),
        crash_loop_signal(snapshot),
        healthcheck_signal(snapshot, latest_healthcheck),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    signals.extend(port_signals(snapshot).await);
    signals
}

pub(crate) async fn diagnose_service(
    conn: &mut SessionConn,
    snapshot: ServiceSnapshot,
    tail: usize,
) -> Result<ServiceDiagnosis, ToolError> {
    let latest_healthcheck = latest_health_for_snapshot(conn, &snapshot)
        .await
        .map(bounded_attempt);
    let signals = supervisor_signals(&snapshot, latest_healthcheck.as_ref()).await;
    let timeline = conn
        .request(Request::GetEvents {
            service: snapshot.id.clone(),
            after: None,
            tail: Some(10),
        })
        .await
        .ok()
        .and_then(|response| convert::events(response).ok())
        .map_or_else(Vec::new, |(events, _)| events);
    let (error_log_tail, logs_truncated) = likely_cause_log_tail(conn, &snapshot, tail).await?;
    let hint = diagnosis_hint(&snapshot, latest_healthcheck.as_ref(), &error_log_tail);
    Ok(ServiceDiagnosis {
        service: snapshot.id.clone(),
        snapshot,
        latest_healthcheck,
        timeline,
        signals,
        error_log_tail,
        logs_truncated,
        hint,
    })
}

pub(crate) async fn service_snapshot(
    conn: &mut SessionConn,
    service: &str,
) -> Result<ServiceSnapshot, ToolError> {
    let response = conn.request(Request::ListServices).await?;
    let services = convert::services(response)?;
    services
        .into_iter()
        .find(|snapshot| snapshot.id == service)
        .ok_or_else(|| ToolError::Remote {
            code: ErrorCode::UnknownService,
            message: format!("unknown service `{service}`"),
        })
}

/// Result of waiting for a caller-defined snapshot condition.
pub(crate) enum SnapshotWait<V> {
    /// The classifier accepted this snapshot.
    Matched {
        /// Snapshot that satisfied the classifier.
        snapshot: ServiceSnapshot,
        /// Classifier result associated with the snapshot.
        verdict: V,
    },
    /// The deadline elapsed while this was the latest snapshot.
    Timeout(ServiceSnapshot),
}

/// Wait for a service snapshot accepted by a caller-defined classifier.
///
/// Subscription notifications are best-effort wakeups; polling remains the lossless fallback.
///
/// # Errors
///
/// Returns a [`ToolError`] when the service snapshot cannot be fetched.
pub(crate) async fn wait_for_snapshot<V>(
    endpoint: &ControlEndpoint,
    conn: &mut SessionConn,
    service: &str,
    timeout: Duration,
    classify: impl Fn(&ServiceSnapshot) -> Option<V>,
) -> Result<SnapshotWait<V>, ToolError> {
    let deadline = tokio::time::Instant::now() + timeout;
    // Best-effort wakeup; `None` (subscribe failed or stream ended) degrades to pure polling.
    let mut subscription = Client::subscribe(endpoint).await.ok();

    loop {
        let snapshot = service_snapshot(conn, service).await?;
        if let Some(verdict) = classify(&snapshot) {
            return Ok(SnapshotWait::Matched { snapshot, verdict });
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(SnapshotWait::Timeout(snapshot));
        }
        let wait = deadline.saturating_duration_since(now).min(WAIT_POLL_FLOOR);

        // Wake on a relevant change (this service; ignore log appends, which don't affect
        // snapshots), but never wait longer than the poll floor before re-polling. Drop the
        // subscription if its stream ends — polling still converges.
        let drop_subscription = if let Some(stream) = subscription.as_mut() {
            let relevant = tokio::time::timeout(wait, async {
                loop {
                    match stream.recv().await {
                        Ok(Some(change))
                            if change.service_id == service && change.kind != ChangeKind::Logs =>
                        {
                            return true;
                        }
                        Ok(Some(_)) => {}
                        Ok(None) | Err(_) => return false,
                    }
                }
            })
            .await;
            matches!(relevant, Ok(false))
        } else {
            tokio::time::sleep(wait).await;
            false
        };
        if drop_subscription {
            subscription = None;
        }
    }
}

pub(crate) async fn wait_for_health(
    endpoint: &ControlEndpoint,
    conn: &mut SessionConn,
    service: &str,
    after_generation: Option<u64>,
    timeout: Duration,
) -> Result<WaitConclusion, ToolError> {
    match wait_for_snapshot(
        endpoint,
        conn,
        service,
        timeout,
        |snapshot| match convert::evaluate(snapshot, after_generation) {
            convert::WaitOutcome::Pending => None,
            convert::WaitOutcome::Healthy => Some(HealthVerdict::Healthy),
            convert::WaitOutcome::Exited(_) => Some(HealthVerdict::Exited),
            convert::WaitOutcome::InvalidState => Some(HealthVerdict::InvalidState),
        },
    )
    .await?
    {
        SnapshotWait::Matched {
            snapshot,
            verdict: HealthVerdict::Healthy,
        } => Ok(WaitConclusion::Healthy(snapshot)),
        SnapshotWait::Matched {
            snapshot,
            verdict: HealthVerdict::Exited,
        } => Ok(WaitConclusion::Exited(snapshot)),
        SnapshotWait::Matched {
            snapshot,
            verdict: HealthVerdict::InvalidState,
        } => {
            let message = if snapshot
                .dynamic
                .as_ref()
                .is_some_and(|dynamic| dynamic.retired.is_some())
            {
                format!("service `{service}` is retired; use replace_dynamic_service to revive it")
            } else {
                format!("service `{service}` is disabled and will not become healthy")
            };
            Err(ToolError::InvalidState(message))
        }
        SnapshotWait::Timeout(snapshot) => {
            let latest_healthcheck = latest_health_for_snapshot(conn, &snapshot)
                .await
                .map(bounded_attempt);
            Ok(WaitConclusion::Timeout {
                snapshot,
                latest_healthcheck,
            })
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

#[cfg(test)]
mod tests {
    use super::*;
    use micromux::{HealthResult, HealthcheckConfig, RestartPolicy, RestartState};
    use similar_asserts::assert_eq;

    fn snapshot(execution: Execution) -> ServiceSnapshot {
        let mut snapshot = ServiceSnapshot::initial(
            "svc".to_string(),
            "svc".to_string(),
            Vec::new(),
            None,
            RestartPolicy::Never,
            Vec::new(),
            None,
        );
        snapshot.execution = execution;
        snapshot.run_generation = 1;
        snapshot
    }

    #[test]
    fn exit_signal_interprets_only_portable_exit_conventions() {
        let cases = [
            (137, "SIGKILL; often OOM"),
            (143, "SIGTERM"),
            (139, "SIGSEGV"),
            (127, "command not found"),
            (126, "command found but not executable"),
            (0, "clean exit"),
        ];
        for (exit_code, expected) in cases {
            let mut snapshot = snapshot(Execution::Exited);
            snapshot.last_exit_code = Some(exit_code);

            let signal = exit_signal(&snapshot);

            assert_eq!(
                signal.as_ref().map(|signal| signal.kind),
                Some(SignalKind::ExitSignal)
            );
            assert!(
                signal
                    .as_ref()
                    .is_some_and(|signal| signal.detail.contains(expected)),
                "missing `{expected}` for exit code {exit_code}"
            );
        }
    }

    #[test]
    fn crash_loop_signal_reports_backoff_and_remaining_budget() {
        let mut snapshot = snapshot(Execution::Exited);
        snapshot.restart_state = Some(RestartState {
            backoff_delay: Duration::from_millis(500),
            restarts_remaining: Some(2),
        });

        let signal = crash_loop_signal(&snapshot);

        assert_eq!(
            signal.as_ref().map(|signal| signal.kind),
            Some(SignalKind::CrashLoop)
        );
        assert!(signal.as_ref().is_some_and(|signal| {
            signal.detail.contains("500ms") && signal.detail.contains('2')
        }));
    }

    #[test]
    fn healthcheck_signal_distinguishes_waiting_from_completed_failure() {
        let mut snapshot = snapshot(Execution::Running);
        snapshot.healthcheck_configured = true;
        snapshot.healthcheck = Some(HealthcheckConfig {
            retries: 3,
            interval: Duration::from_secs(2),
            timeout: Duration::from_secs(1),
        });

        let waiting = healthcheck_signal(&snapshot, None);
        assert!(waiting.as_ref().is_some_and(|signal| {
            signal.kind == SignalKind::HealthcheckTimeout
                && signal
                    .detail
                    .contains("no healthcheck attempt has completed yet")
                && signal.detail.contains("retries: 3")
        }));

        let attempt = HealthAttempt {
            run_generation: 1,
            attempt: 1,
            command: "probe".to_string(),
            output: Vec::new(),
            result: Some(HealthResult {
                success: false,
                exit_code: 7,
                cancelled: false,
            }),
        };
        let failing = healthcheck_signal(&snapshot, Some(&attempt));
        assert!(failing.as_ref().is_some_and(|signal| {
            signal.detail.contains("failed with exit status 7")
                && signal.detail.contains("interval: 2s")
                && signal.detail.contains("timeout: 1s")
        }));
    }

    #[tokio::test]
    async fn held_advertised_port_emits_unavailable_signal() -> color_eyre::Result<()> {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let mut snapshot = snapshot(Execution::Exited);
        snapshot.advertised_ports = vec![port];

        let signals = port_signals(&snapshot).await;

        assert_eq!(signals.len(), 1);
        assert_eq!(
            signals.first().map(|signal| signal.kind),
            Some(SignalKind::PortUnavailable)
        );

        // A draining service plausibly holds its own port — probing would blame the service for
        // its own bind, so live-process states must not emit the signal.
        snapshot.execution = Execution::Stopping;
        assert_eq!(port_signals(&snapshot).await, Vec::new());
        Ok(())
    }

    #[test]
    fn non_address_in_use_probe_errors_do_not_emit_signals() {
        let error = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(classify_bind_result(Err(error)), PortProbe::Unknown);
    }
}
