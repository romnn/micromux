//! The `micromux ctl` client: a thin dogfood of the control protocol for humans and scripts.

use std::path::{Path, PathBuf};

use micromux_control::{
    Client, ControlEndpoint, EndpointProbe, EndpointProbeResult, Request, Response,
    RuntimeDirStatus, SessionInfo, answering_session_probes, endpoint_for, probe_endpoints,
    runtime_dir_statuses, unique_answering_session_probes, usable_runtime_dirs,
};

use micromux_control::wait::{self, ChangeWaker, WaitFor, WaitVerdict};

use crate::options::CtlAction;

fn request_for(action: &CtlAction) -> Request {
    match action {
        CtlAction::Ls => Request::ListServices,
        CtlAction::Logs {
            service,
            run_generation,
            tail,
        } => Request::GetLogs {
            service: service.clone(),
            run_generation: *run_generation,
            tail: *tail,
        },
        CtlAction::LogRuns { service } => Request::ListLogRuns {
            service: service.clone(),
        },
        CtlAction::Restart { service } => Request::Restart {
            service: service.clone(),
        },
        CtlAction::RestartAll => Request::RestartAll,
        CtlAction::Enable { service } => Request::Enable {
            service: service.clone(),
        },
        CtlAction::Disable { service } => Request::Disable {
            service: service.clone(),
        },
        CtlAction::StopDynamic { service } => Request::StopDynamicService {
            service: service.clone(),
        },
        CtlAction::Reconcile { dry_run } => Request::ReconcileConfig { dry_run: *dry_run },
        CtlAction::Health {
            service,
            history: false,
        } => Request::GetHealth {
            service: service.clone(),
        },
        CtlAction::Health {
            service,
            history: true,
        } => Request::GetHealthHistory {
            service: service.clone(),
        },
        // A wait polls the service; `wait` drives the loop.
        CtlAction::Wait { service, .. } => Request::GetService {
            service: service.clone(),
        },
        CtlAction::Describe => Request::Describe,
        CtlAction::Stop => Request::Shutdown,
    }
}

fn service_origin_label(service: &micromux::ServiceSnapshot) -> String {
    match (&service.origin, &service.dynamic) {
        (micromux::OriginKind::Configured, _) => "configured".to_string(),
        (micromux::OriginKind::Dynamic, Some(dynamic)) => {
            format!("dynamic(revision={})", dynamic.revision)
        }
        (micromux::OriginKind::Dynamic, None) => "dynamic".to_string(),
        (micromux::OriginKind::Unknown, _) => "unknown".to_string(),
    }
}

/// Retirement suffix for a service row. Origin-independent: a reconcile-removed configured
/// service must be as visible as a stopped dynamic one.
fn service_retired_label(service: &micromux::ServiceSnapshot) -> String {
    match service.retired {
        Some(micromux::RetiredReason::Stopped) => " retired=stopped".to_string(),
        Some(micromux::RetiredReason::Expired) => " retired=expired".to_string(),
        Some(micromux::RetiredReason::Removed) => " retired=removed".to_string(),
        Some(micromux::RetiredReason::Unknown) => " retired=unknown".to_string(),
        None => String::new(),
    }
}

/// Prints the session's hostnames and allocated ports.
fn print_session(session: &micromux::SessionSummary) {
    println!("  session: {}", session.name);
    if !session.routes.is_empty() {
        let routing = match &session.routing {
            Some(micromux::RoutingStatus::Unavailable { reason }) => {
                format!(" (unavailable: {reason})")
            }
            Some(micromux::RoutingStatus::Pending) => " (not registered yet)".to_string(),
            Some(micromux::RoutingStatus::Registered) | None => String::new(),
        };
        println!("  urls:{routing}");
        for route in &session.routes {
            let service = route
                .service
                .as_deref()
                .map(|service| format!(" ({service})"))
                .unwrap_or_default();
            let stripped = if route.strip_prefix {
                ", prefix stripped"
            } else {
                ""
            };
            println!(
                "    - {} -> 127.0.0.1:{}{stripped}{service}",
                route.url, route.port
            );
        }
    }
    if !session.allocated_ports.is_empty() {
        println!("  ports:");
        for (name, port) in &session.allocated_ports {
            println!("    - {name}={port}");
        }
    }
}

/// Suffix marking a job, and a manual one.
fn service_kind_label(service: &micromux::ServiceSnapshot) -> &'static str {
    match (service.kind, service.manual) {
        (micromux::ServiceKind::Service, _) => "",
        (micromux::ServiceKind::Job, false) => " kind=job",
        (micromux::ServiceKind::Job, true) => " kind=job run=manual",
    }
}

/// Suffix with the exit code of a job that failed.
fn job_exit_label(service: &micromux::ServiceSnapshot) -> String {
    match (service.execution, service.last_exit_code) {
        (micromux::Execution::Failed, Some(code)) => format!(" exit_code={code}"),
        _ => String::new(),
    }
}

/// Suffix marking a service whose config changed since its latest run started, so a restart
/// would apply it.
fn config_stale_label(service: &micromux::ServiceSnapshot) -> &'static str {
    if service.config_stale {
        " config=stale"
    } else {
        ""
    }
}

/// Suffix naming the dependency that holds a blocked service back for good.
fn dependency_failure_label(service: &micromux::ServiceSnapshot) -> String {
    let Some(failure) = &service.dependency_failure else {
        return String::new();
    };
    let reason = match failure.reason {
        micromux::DependencyFailureReason::Exited => "exited",
        micromux::DependencyFailureReason::Disabled => "disabled",
        micromux::DependencyFailureReason::Retired => "retired",
        micromux::DependencyFailureReason::Missing => "missing",
        micromux::DependencyFailureReason::NoHealthcheck => "no_healthcheck",
        micromux::DependencyFailureReason::NotRun => "not_run",
        micromux::DependencyFailureReason::Unknown => "unknown",
    };
    let exit_code = failure
        .exit_code
        .map(|code| format!(",exit_code={code}"))
        .unwrap_or_default();
    format!(
        " failed_dependency={}({reason}{exit_code})",
        failure.service
    )
}

fn print_services(services: &[micromux::ServiceSnapshot]) {
    for service in services {
        let health = service
            .health
            .map_or_else(|| "-".to_string(), |health| health.to_string());
        println!(
            "{:<20} id={} origin={}{} desired={:?} execution={:?}{} health={} generation={}{}{}{}",
            service.name,
            service.id,
            service_origin_label(service),
            service_kind_label(service),
            service.desired,
            service.execution,
            job_exit_label(service),
            health,
            service.run_generation,
            config_stale_label(service),
            service_retired_label(service),
            dependency_failure_label(service)
        );
    }
}

fn dynamic_receipt_line(receipt: &micromux_control::DynamicServiceAck) -> String {
    format!(
        "accepted {} (revision {}, generation {}, already_retired={})",
        receipt.service, receipt.revision, receipt.observed_generation, receipt.already_retired
    )
}

fn print_health_attempt(attempt: &micromux::HealthAttempt) {
    println!(
        "attempt {} `{}` -> {}",
        attempt.attempt,
        attempt.command,
        attempt.result.map_or_else(
            || "running".to_string(),
            |result| format!("success={} exit_code={}", result.success, result.exit_code)
        )
    );
    for line in &attempt.output {
        println!("  {}", line.line);
    }
}

fn print_response(response: &Response) -> Result<(), crate::Error> {
    match response {
        Response::Services(services) => print_services(services),
        Response::Service(service) => print_services(std::slice::from_ref(service.as_ref())),
        Response::Logs {
            lines, truncated, ..
        } => {
            for line in lines {
                println!("{}", line.line);
            }
            if *truncated {
                eprintln!("log response truncated by server limits");
            }
        }
        Response::LogRuns { runs } => {
            for run in runs {
                println!(
                    "generation={} current={} lines={} seq={:?}..{:?}",
                    run.run_generation, run.current, run.line_count, run.first_seq, run.last_seq
                );
            }
        }
        Response::Health(Some(attempt)) => {
            print_health_attempt(attempt);
        }
        Response::HealthHistory { attempts } => {
            if attempts.is_empty() {
                println!("no healthcheck attempts recorded");
            }
            for attempt in attempts {
                print_health_attempt(attempt);
            }
        }
        Response::Health(None) => println!("no healthcheck attempts recorded"),
        Response::Events { events, truncated } => {
            for event in events {
                println!("{} {:?}: {}", event.seq, event.kind, event.detail);
            }
            if *truncated {
                eprintln!("event response truncated by server limits");
            }
        }
        Response::Description(info) => {
            println!("{} (pid {})", info.name, info.pid);
            println!("  config:  {}", info.config_path);
            println!("  cwd:     {}", info.working_dir);
            println!("  version: {}", info.micromux_version);
            if let Some(session) = &info.session {
                print_session(session);
            }
            println!("  services:");
            for service in &info.services {
                println!("    - {} ({})", service.name, service.id);
            }
            if info.services_truncated {
                println!("    … service index truncated by server limits");
            }
        }
        Response::Accepted { services } => {
            if services.is_empty() {
                println!("accepted (no services affected)");
            }
            for ack in services {
                println!(
                    "accepted {} (generation {})",
                    ack.service, ack.observed_generation
                );
            }
        }
        Response::DynamicService(receipt) => {
            println!("{}", dynamic_receipt_line(receipt));
        }
        Response::Reconcile(receipt) => {
            println!(
                "reconcile {}{}",
                receipt.config_path,
                if receipt.dry_run { " (dry run)" } else { "" }
            );
            if receipt.actions.is_empty() {
                println!("  no changes");
            }
            for action in &receipt.actions {
                let action_name = match action.action {
                    micromux::ReconcileActionKind::Added => "added",
                    micromux::ReconcileActionKind::Removed => "removed",
                    micromux::ReconcileActionKind::Changed => "changed",
                };
                println!("  {action_name:<7} {}: {}", action.service, action.detail);
            }
        }
        Response::Error { code, message } => {
            return Err(crate::Error::Message(format!("{code:?}: {message}")));
        }
        Response::ShuttingDown => {
            println!("session is shutting down");
        }
        // `ctl` never subscribes, asks for environment values, or drives service terminals.
        Response::Change(_) | Response::Environment(_) | Response::TerminalQueued => {}
    }
    Ok(())
}

fn current_exe_label() -> String {
    std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string())
}

fn format_session(endpoint: &ControlEndpoint, info: &SessionInfo) -> String {
    format!(
        "{} -> session name={} pid={} config={}",
        endpoint, info.name, info.pid, info.config_path
    )
}

fn format_probe(probe: &EndpointProbe) -> String {
    let result = match &probe.result {
        EndpointProbeResult::Session(info) => return format_session(&probe.endpoint, info),
        EndpointProbeResult::Absent(reason) => format!("absent ({reason})"),
        EndpointProbeResult::ProtocolMismatch { peer, ours } => {
            format!("protocol_mismatch (peer={peer}, ours={ours})")
        }
        EndpointProbeResult::Unreachable(reason) => format!("unreachable ({reason})"),
    };
    format!("{} -> {result}", probe.endpoint)
}

fn format_runtime_dirs(dir_statuses: &[RuntimeDirStatus]) -> String {
    let searched = dir_statuses
        .iter()
        .map(|status| {
            if status.usable {
                format!("{} (usable)", status.path.display())
            } else {
                format!(
                    "{} (unusable: {})",
                    status.path.display(),
                    status.error.as_deref().unwrap_or("unknown error"),
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n  ");
    if searched.is_empty() {
        "none".to_string()
    } else {
        searched
    }
}

fn diagnostic_context(
    dir_statuses: &[RuntimeDirStatus],
    config_path: &Path,
    working_dir: &Path,
) -> String {
    format!(
        "executable: {} (version {}, protocol {})\n\
         cwd: {}\n\
         config_path: {}\n\
         XDG_RUNTIME_DIR: {}\n\
         runtime_dirs:\n  {}",
        current_exe_label(),
        env!("CARGO_PKG_VERSION"),
        micromux_control::PROTOCOL_VERSION,
        working_dir.display(),
        config_path.display(),
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "<unset>".to_string()),
        format_runtime_dirs(dir_statuses),
    )
}

fn no_session_message(
    dir_statuses: &[RuntimeDirStatus],
    probes: &[EndpointProbe],
    config_path: &Path,
    working_dir: &Path,
) -> String {
    let summary = if probes.iter().any(|probe| {
        matches!(
            probe.result,
            EndpointProbeResult::ProtocolMismatch { .. } | EndpointProbeResult::Unreachable(_)
        )
    }) {
        format!(
            "no answering micromux session for {}; at least one endpoint was reachable but unusable",
            config_path.display()
        )
    } else {
        format!("no running micromux session for {}", config_path.display())
    };
    let probe_lines = if probes.is_empty() {
        "none".to_string()
    } else {
        probes
            .iter()
            .map(format_probe)
            .collect::<Vec<_>>()
            .join("\n  ")
    };
    format!(
        "{summary}\n\
         {}\n\
         socket_probes:\n  {}",
        diagnostic_context(dir_statuses, config_path, working_dir),
        probe_lines,
    )
}

fn ambiguous_session_message(
    dir_statuses: &[RuntimeDirStatus],
    sessions: &[(ControlEndpoint, SessionInfo)],
    config_path: &Path,
    working_dir: &Path,
) -> String {
    let session_lines = sessions
        .iter()
        .map(|(endpoint, info)| format_session(endpoint, info))
        .collect::<Vec<_>>()
        .join("\n  ");
    format!(
        "multiple micromux sessions answer for {}; refusing to choose one\n\
         {}\n\
         matching_sessions:\n  {}",
        config_path.display(),
        diagnostic_context(dir_statuses, config_path, working_dir),
        session_lines,
    )
}

/// A connection to the session that runs the project's config.
pub(crate) struct ProjectSession {
    pub(crate) endpoint: ControlEndpoint,
    pub(crate) client: Client,
    pub(crate) info: SessionInfo,
}

/// Connects to the session for the config found at or above the current directory, or for
/// `config_path`.
///
/// Only that config's endpoints are probed, so a session for any other config never answers,
/// not even one whose config lives under the current directory.
pub(crate) async fn connect_project(
    config_path: Option<&Path>,
) -> Result<ProjectSession, crate::Error> {
    if !micromux_control::transport_supported() {
        return Err(crate::Error::Message(
            "the micromux control plane is not supported on this platform".to_string(),
        ));
    }

    let working_dir = std::env::current_dir()?;
    let config_path = crate::control::resolve_config_path(config_path, &working_dir).await?;
    let dir_statuses = runtime_dir_statuses();
    let runtime_dirs = usable_runtime_dirs(&dir_statuses);
    if runtime_dirs.is_empty() {
        return Err(crate::Error::Message(no_session_message(
            &dir_statuses,
            &[],
            &config_path,
            &working_dir,
        )));
    }
    connect_project_session(&dir_statuses, &runtime_dirs, &working_dir, &config_path).await
}

async fn connect_project_session(
    dir_statuses: &[RuntimeDirStatus],
    runtime_dirs: &[PathBuf],
    working_dir: &Path,
    config_path: &micromux_control::CanonicalConfigPath,
) -> Result<ProjectSession, crate::Error> {
    let endpoints = runtime_dirs
        .iter()
        .map(|runtime_dir| endpoint_for(runtime_dir, config_path))
        .collect::<Vec<_>>();
    let probes = probe_endpoints(&endpoints).await;
    let session_probes = answering_session_probes(&probes);
    let unique_sessions = unique_answering_session_probes(&probes);
    let mut failed_probes = probes
        .into_iter()
        .filter(|probe| !matches!(probe.result, EndpointProbeResult::Session(_)))
        .collect::<Vec<_>>();

    if unique_sessions.len() > 1 {
        return Err(crate::Error::Message(ambiguous_session_message(
            dir_statuses,
            &unique_sessions,
            config_path,
            working_dir,
        )));
    }

    if let Some((_, selected)) = unique_sessions.into_iter().next() {
        for (endpoint, info) in session_probes {
            if !info.is_same_instance(&selected) {
                continue;
            }
            match Client::connect(&endpoint).await {
                Ok(client) => {
                    return Ok(ProjectSession {
                        endpoint,
                        client,
                        info,
                    });
                }
                Err(err) => failed_probes.push(EndpointProbe {
                    endpoint,
                    result: EndpointProbeResult::Unreachable(format!(
                        "connect after Describe: {err}"
                    )),
                }),
            }
        }
    }

    Err(crate::Error::Message(no_session_message(
        dir_statuses,
        &failed_probes,
        config_path,
        working_dir,
    )))
}

/// Connect to the current project's session endpoint and run a single `ctl` action.
///
/// # Errors
///
/// Returns an error if no session is running for this project, the runtime dir is unresolvable, or
/// the request fails.
pub async fn run(action: CtlAction, config_path: Option<&Path>) -> Result<(), crate::Error> {
    let ProjectSession {
        endpoint,
        mut client,
        ..
    } = connect_project(config_path).await?;

    if let CtlAction::Wait {
        exit,
        timeout,
        after_generation,
        ..
    } = &action
    {
        let wait_for = if *exit {
            WaitFor::Exit
        } else {
            WaitFor::Healthy
        };
        return wait(
            &endpoint,
            &mut client,
            &request_for(&action),
            wait_for,
            *timeout,
            *after_generation,
        )
        .await;
    }

    let response = client.request(request_for(&action)).await?;
    print_response(&response)
}

/// Poll a service with `request` until the wait resolves or `timeout` elapses.
async fn wait(
    endpoint: &ControlEndpoint,
    client: &mut Client,
    request: &Request,
    wait_for: WaitFor,
    timeout: std::time::Duration,
    after_generation: Option<u64>,
) -> Result<(), crate::Error> {
    let Request::GetService { service } = request else {
        return Err(crate::Error::Message(format!(
            "a wait polls with a service query, not {request:?}"
        )));
    };
    let deadline = tokio::time::Instant::now() + timeout;
    let mut waker = ChangeWaker::subscribe(endpoint, service, WAIT_POLL).await;
    loop {
        let snapshot = match client.request(request.clone()).await? {
            Response::Service(snapshot) => *snapshot,
            Response::Error { code, message } => {
                return Err(crate::Error::Message(format!("{code:?}: {message}")));
            }
            other => {
                return Err(crate::Error::Message(format!(
                    "unexpected response to a service query: {other:?}"
                )));
            }
        };
        if let Some(verdict) = wait::evaluate(&snapshot, wait_for, after_generation) {
            return report_wait(&snapshot, wait_for, &verdict);
        }
        if tokio::time::Instant::now() >= deadline {
            eprintln!(
                "timed out after {} waiting for {}; it is {:?} at generation {}{}",
                humantime::format_duration(timeout),
                snapshot.id,
                snapshot.execution,
                snapshot.run_generation,
                snapshot
                    .health
                    .map(|health| format!(" ({health})"))
                    .unwrap_or_default()
            );
            return Err(crate::Error::Exit(WAIT_TIMED_OUT));
        }
        waker.sleep(deadline).await;
    }
}

/// Longest a `ctl wait` sleeps between polls when no change notification wakes it.
const WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(500);
/// Exit code of a `ctl wait` whose run exited instead of meeting the condition.
const WAIT_EXITED: i32 = 3;
/// Exit code of a `ctl wait` on a service blocked behind a failed dependency.
const WAIT_DEPENDENCY_FAILED: i32 = 4;
/// Exit code of a `ctl wait` on a disabled or retired service.
const WAIT_UNAVAILABLE: i32 = 5;
/// Exit code of a `ctl wait` that timed out, as `timeout(1)` uses.
const WAIT_TIMED_OUT: i32 = 124;

/// Print how a `ctl wait` resolved and map it to the process exit code.
fn report_wait(
    snapshot: &micromux::ServiceSnapshot,
    wait_for: WaitFor,
    verdict: &WaitVerdict,
) -> Result<(), crate::Error> {
    let service = &snapshot.id;
    let generation = snapshot.run_generation;
    let exit_code = |code: Option<i32>| {
        code.map_or_else(
            || "an unknown status".to_string(),
            |code| format!("code {code}"),
        )
    };
    match (verdict, wait_for) {
        (WaitVerdict::Healthy, _) => {
            println!("{service} is healthy (generation {generation})");
            Ok(())
        }
        (WaitVerdict::Completed, _) => {
            println!("{service} completed (generation {generation})");
            Ok(())
        }
        (WaitVerdict::Exited(Some(0)), WaitFor::Exit) => {
            println!("{service} exited with code 0 (generation {generation})");
            Ok(())
        }
        (WaitVerdict::Exited(code), _) => {
            eprintln!(
                "{service} exited with {} (generation {generation})",
                exit_code(*code)
            );
            Err(crate::Error::Exit(WAIT_EXITED))
        }
        (WaitVerdict::DependencyFailed(failure), _) => {
            eprintln!("{}", wait::dependency_failure_message(service, failure));
            Err(crate::Error::Exit(WAIT_DEPENDENCY_FAILED))
        }
        (WaitVerdict::Idle, _) => {
            if snapshot.execution == micromux::Execution::Pending {
                eprintln!(
                    "{service} is a manual job that has not run; run it with `micromux ctl restart {service}`"
                );
            } else {
                eprintln!(
                    "{service} finished at generation {generation} and runs again only when restarted; \
                     pass the generation `ctl restart` prints as --after-generation"
                );
            }
            Err(crate::Error::Exit(WAIT_UNAVAILABLE))
        }
        (WaitVerdict::Unavailable, _) => {
            let state = if snapshot.retired.is_some() {
                "retired"
            } else {
                "disabled"
            };
            eprintln!("{service} is {state} and has no run in flight to wait for");
            Err(crate::Error::Exit(WAIT_UNAVAILABLE))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{dynamic_receipt_line, request_for, service_origin_label, service_retired_label};
    use crate::options::CtlAction;
    use micromux_control::{DynamicServiceAck, Request};
    use similar_asserts::assert_eq;

    #[test]
    fn reconcile_maps_to_the_control_request_with_dry_run() {
        assert!(matches!(
            request_for(&CtlAction::Reconcile { dry_run: true }),
            Request::ReconcileConfig { dry_run: true }
        ));
        assert!(matches!(
            request_for(&CtlAction::Reconcile { dry_run: false }),
            Request::ReconcileConfig { dry_run: false }
        ));
    }

    #[test]
    fn health_maps_to_latest_or_history_by_flag() {
        assert!(matches!(
            request_for(&CtlAction::Health {
                service: "api".to_string(),
                history: false,
            }),
            Request::GetHealth { service } if service == "api"
        ));
        assert!(matches!(
            request_for(&CtlAction::Health {
                service: "api".to_string(),
                history: true,
            }),
            Request::GetHealthHistory { service } if service == "api"
        ));
    }

    #[test]
    fn retirement_is_visible_for_every_origin_without_debug_formatting() {
        let mut snapshot = micromux::ServiceSnapshot::initial(
            "removed".to_string(),
            "removed".to_string(),
            Vec::new(),
            None,
            micromux::RestartPolicy::Never,
            vec!["true".to_string()],
            None,
        );
        assert_eq!(service_retired_label(&snapshot), "");

        // A reconcile-removed configured service must be as visible as a stopped dynamic one.
        snapshot.retired = Some(micromux::RetiredReason::Removed);
        assert_eq!(service_origin_label(&snapshot), "configured");
        assert_eq!(service_retired_label(&snapshot), " retired=removed");

        snapshot.origin = micromux::OriginKind::Dynamic;
        snapshot.dynamic = Some(micromux::DynamicServiceInfo {
            created_at_unix_ms: 0,
            expires_at_unix_ms: None,
            owner: None,
            revision: 2,
        });
        snapshot.retired = Some(micromux::RetiredReason::Stopped);
        assert_eq!(service_origin_label(&snapshot), "dynamic(revision=2)");
        assert_eq!(service_retired_label(&snapshot), " retired=stopped");
    }

    #[test]
    fn stop_dynamic_maps_to_the_control_request_and_prints_retirement_state() {
        let request = request_for(&CtlAction::StopDynamic {
            service: "debug".to_string(),
        });
        assert!(matches!(
            request,
            Request::StopDynamicService { service } if service == "debug"
        ));

        let line = dynamic_receipt_line(&DynamicServiceAck {
            service: "debug".to_string(),
            revision: 2,
            observed_generation: 3,
            expires_at_unix_ms: None,
            command: vec!["true".to_string()],
            working_dir: None,
            ports: Vec::new(),
            env_keys: Vec::new(),
            restart: micromux::RestartPolicy::Never,
            healthcheck_configured: false,
            already_retired: true,
            idempotent_replay: false,
        });
        assert_eq!(
            line,
            "accepted debug (revision 2, generation 3, already_retired=true)"
        );
    }
}
