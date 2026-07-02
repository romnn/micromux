//! The `micromux ctl` client: a thin dogfood of the control protocol for humans and scripts.

use std::path::{Path, PathBuf};

use color_eyre::eyre;
use micromux_control::{
    Client, ControlEndpoint, EndpointProbe, EndpointProbeResult, Request, Response,
    RuntimeDirStatus, SessionInfo, answering_session_probes, endpoint_for, probe_endpoints,
    runtime_dir_statuses, unique_answering_session_probes, usable_runtime_dirs,
};

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
        CtlAction::Health { service } => Request::GetHealth {
            service: service.clone(),
        },
        CtlAction::Describe => Request::Describe,
        CtlAction::Stop => Request::Shutdown,
    }
}

fn print_response(response: &Response) -> eyre::Result<()> {
    match response {
        Response::Services(services) => {
            for service in services {
                let health = service
                    .health
                    .map_or_else(|| "-".to_string(), |health| health.to_string());
                println!(
                    "{:<20} desired={:?} execution={:?} health={} generation={}",
                    service.name,
                    service.desired,
                    service.execution,
                    health,
                    service.run_generation
                );
            }
        }
        Response::Logs { lines, truncated } => {
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
        Response::Health(None) => println!("no healthcheck attempts recorded"),
        Response::Description(info) => {
            println!("{} (pid {})", info.name, info.pid);
            println!("  config:  {}", info.config_path);
            println!("  cwd:     {}", info.working_dir);
            println!("  version: {}", info.micromux_version);
            println!("  services:");
            for service in &info.services {
                println!("    - {} ({})", service.name, service.id);
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
        Response::Error { code, message } => {
            eyre::bail!("{code:?}: {message}");
        }
        Response::ShuttingDown => {
            println!("session is shutting down");
        }
        Response::Change(_) => {}
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
        "executable: {} (version {})\n\
         cwd: {}\n\
         config_path: {}\n\
         XDG_RUNTIME_DIR: {}\n\
         runtime_dirs:\n  {}",
        current_exe_label(),
        env!("CARGO_PKG_VERSION"),
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
    let summary = if probes
        .iter()
        .any(|probe| matches!(probe.result, EndpointProbeResult::Unreachable(_)))
    {
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

async fn connect_project_session(
    dir_statuses: &[RuntimeDirStatus],
    runtime_dirs: &[PathBuf],
    working_dir: &Path,
    config_path: &Path,
) -> eyre::Result<Client> {
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
        eyre::bail!(
            "{}",
            ambiguous_session_message(dir_statuses, &unique_sessions, config_path, working_dir)
        );
    }

    if let Some((_, selected)) = unique_sessions.into_iter().next() {
        for (endpoint, info) in session_probes {
            if !info.is_same_instance(&selected) {
                continue;
            }
            match Client::connect(&endpoint).await {
                Ok(client) => return Ok(client),
                Err(err) => failed_probes.push(EndpointProbe {
                    endpoint,
                    result: EndpointProbeResult::Unreachable(format!(
                        "connect after Describe: {err}"
                    )),
                }),
            }
        }
    }

    eyre::bail!(
        "{}",
        no_session_message(dir_statuses, &failed_probes, config_path, working_dir)
    );
}

/// Connect to the current project's session endpoint and run a single `ctl` action.
///
/// # Errors
///
/// Returns an error if no session is running for this project, the runtime dir is unresolvable, or
/// the request fails.
pub async fn run(action: CtlAction, config_path: Option<&Path>) -> eyre::Result<()> {
    if !micromux_control::transport_supported() {
        eyre::bail!("the micromux control plane is not supported on this platform");
    }

    let working_dir = std::env::current_dir()?;
    let config_path = crate::control::resolve_config_path(config_path, &working_dir).await?;
    let dir_statuses = runtime_dir_statuses();
    let runtime_dirs = usable_runtime_dirs(&dir_statuses);
    if runtime_dirs.is_empty() {
        eyre::bail!(
            "{}",
            no_session_message(&dir_statuses, &[], &config_path, &working_dir)
        );
    }
    let mut client =
        connect_project_session(&dir_statuses, &runtime_dirs, &working_dir, &config_path).await?;

    let response = client.request(request_for(&action)).await?;
    print_response(&response)
}
