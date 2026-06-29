//! The `micromux ctl` client: a thin dogfood of the control protocol for humans and scripts.

use std::path::Path;

use color_eyre::eyre;
use micromux_control::{Client, Request, Response, endpoint_for, runtime_dir};

use crate::options::CtlAction;

fn request_for(action: &CtlAction) -> Request {
    match action {
        CtlAction::Ls => Request::ListServices,
        CtlAction::Logs { service, tail } => Request::GetLogs {
            service: service.clone(),
            tail: *tail,
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
        Response::Logs { lines } => {
            for line in lines {
                println!("{}", line.line);
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
        Response::Change(_) => {}
    }
    Ok(())
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
    let runtime_dir =
        runtime_dir().ok_or_else(|| eyre::eyre!("no runtime directory could be resolved"))?;
    let endpoint = endpoint_for(&runtime_dir, &config_path);

    let mut client = Client::connect(&endpoint).await.map_err(|err| {
        eyre::eyre!("no micromux session for this project ({err}); is micromux running here?")
    })?;

    let response = client.request(request_for(&action)).await?;
    print_response(&response)
}
