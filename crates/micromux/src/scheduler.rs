use crate::{
    graph::ServiceGraph,
    health_check::Health,
    service::{self, Service},
    shutdown,
};
use color_eyre::eyre;
use futures::FutureExt;
use std::collections::HashMap;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

pub type ServiceID = String;

// Healthy,
// Unhealthy,

#[derive(Debug, strum::Display, strum::IntoStaticStr)]
pub enum State {
    /// Service has not yet started.
    #[strum(serialize = "PENDING")]
    Pending,
    /// Service is running.
    #[strum(serialize = "RUNNING")]
    Running {
        // process: async_process::Child,
        health: Option<Health>,
    },
    /// Service is disabled.
    #[strum(serialize = "DISABLED")]
    Disabled,
    /// Service exited with code.
    #[strum(serialize = "EXITED")]
    Exited {
        exit_code: i32,
        restart_policy: service::RestartPolicy,
    },
    /// Service has been killed and is awaiting exit
    #[strum(serialize = "KILLED")]
    Killed,
}

#[derive(Debug, Clone)]
pub enum Event {
    Started(ServiceID),
    Killed(ServiceID),
    Exited(ServiceID, i32),
    Healthy(ServiceID),
    Unhealthy(ServiceID),
    Disabled(ServiceID),
}

/// Start service.
async fn start_service(
    service: &Service,
    events_tx: mpsc::Sender<Event>,
    // shutdown: crate::shutdown::Shutdown,
    // mut shutdown_handle: crate::shutdown::Handle,
    cancel: CancellationToken,
) -> eyre::Result<()> {
    use async_process::{Command, Stdio};
    use futures::{AsyncBufReadExt, StreamExt};

    let service_id = service.id.clone();

    // let args: Vec<String> = shlex::split(&service.command).unwrap_or_default();
    // let Some((program, program_args)) = args.split_first() else {
    //     eyre::bail!("bad command: {:?}", service.command);
    // };
    let (prog, args) = &service.command;
    let mut process = Command::new(prog)
        .args(args)
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    // TODO: send output of program somewhere...
    // if let Some(stderr) = process.stderr.take() {
    //     // let mut lines = tokio::io::BufReader::new(stdout).lines();
    //     let mut lines = futures::io::BufReader::new(stderr).lines();
    //
    //     while let Some(line) = lines.next().await {
    //         // println!("{}", line?);
    //     }
    // }
    //
    // if let Some(stdout) = process.stdout.take() {
    //     // let mut lines = tokio::io::BufReader::new(stdout).lines();
    //     let mut lines = futures::io::BufReader::new(stdout).lines();
    //
    //     while let Some(line) = lines.next().await {
    //         // println!("{}", line?);
    //     }
    // }

    // let mut cmd = async_process::Command::new(&service.command[0]);
    // cmd.args(&service.command[1..]);
    // let mut child = cmd.spawn().expect("spawn failed");

    let _ = events_tx.send(Event::Started(service_id.clone())).await;

    // Monitor for exit or shutdown
    tokio::spawn({
        let events_tx = events_tx.clone();
        let service_id = service_id.clone();
        let cancel = cancel.clone();
        async move {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!(pid = process.id(), "killing process");
                    // Kill the process
                    let _ = events_tx.send(Event::Killed(service_id.clone())).await;
                    let _ = process.kill();
                    // Optionally wait for it to actually exit
                    let _ = process.status().await;
                    let _ = events_tx.send(Event::Exited(service_id.clone(), -1)).await;
                }
                status = process.status() => {
                    // Process exited by itself
                    match status {
                        Ok(status) => {
                            let code = status.code().unwrap_or(-1);
                            let _ = events_tx.send(Event::Exited(service_id.clone(), code)).await;
                        },
                        Err(err) => {
                            tracing::error!(?err, "failed to get process status");
                        }
                    }
                }

            }
        }
    });

    // If the service specifies a health check, start the health check loop
    if let Some(health_check) = service.health_check.clone() {
        tokio::spawn({
            let service_id = service_id.clone();
            async move {
                let _ = health_check.run_loop(&service_id, events_tx, cancel).await;
            }
        });
    }
    Ok(())
}

async fn schedule_ready(
    services: &HashMap<ServiceID, Service>,
    graph: &petgraph::graphmap::DiGraphMap<&str, ()>,
    service_state: &mut HashMap<ServiceID, State>,
    // events_rx: &mpsc::Receiver<Event>,
    events_tx: &mpsc::Sender<Event>,
    broadcast_tx: &broadcast::Sender<Event>,
    // shutdown_handle: &crate::shutdown::Handle,
    cancel: &CancellationToken,
) {
    use crate::{config::DependencyCondition, service::RestartPolicy};

    // Find services that are ready to start
    for (service_id, service) in services {
        tracing::trace!(service_id, state = ?service.state, "evaluating service");

        let state = service_state.get_mut(service_id.as_str()).unwrap();

        // Check if service should be (re)started
        match state {
            State::Pending => {
                // Proceed to check if service is ready to be started
            }
            State::Running { .. } | State::Killed | State::Disabled => {
                // Skip disabled or already running service
                // Killed processes will eventually exit and become ready for restart.
                continue;
            }
            State::Exited { restart_policy, .. } => match restart_policy {
                RestartPolicy::Never => {
                    // Skip restarting exited container
                    continue;
                }
                RestartPolicy::OnFailure { remaining_attempts } if *remaining_attempts <= 0 => {
                    // Skip restarting exited container when no more attempts remaining
                    continue;
                }
                // TODO: we should keep all runtime state separate?
                RestartPolicy::OnFailure { remaining_attempts } => {
                    // Decrement remaining attempts
                    *remaining_attempts = remaining_attempts.saturating_sub(1);
                }
                RestartPolicy::UnlessStopped | RestartPolicy::Always => {
                    // Proceed to check if service is ready to be restarted
                }
            },
        }

        // Only start if not already Running/Healthy
        // if matches!(state, State::Pending | State::Exited { .. }) {
        // Find dependencies
        let mut dependencies = graph.neighbors_directed(service_id, petgraph::Incoming);

        // All dependencies must be ready
        let is_ready = dependencies.all(|dep| {
            let condition = service
                .depends_on
                .iter()
                .find(|dep_config| dep_config.name.as_ref() == dep)
                .and_then(|dep_config| dep_config.condition.as_ref())
                .map(|condition| condition.as_ref())
                .copied()
                .unwrap_or_default();
            let state = &service_state[dep];
            let is_ready = match condition {
                DependencyCondition::ServiceStarted => {
                    matches!(state, State::Running { .. })
                }
                DependencyCondition::ServiceHealthy => {
                    matches!(
                        state,
                        State::Running {
                            health: Some(Health::Healthy),
                            ..
                        }
                    )
                }
                DependencyCondition::ServiceCompletedSuccessfully => {
                    matches!(state, State::Exited { exit_code: 0, .. })
                }
            };
            is_ready
        });
        if is_ready {
            tracing::info!("starting {}", service_id);
            start_service(service, events_tx.clone(), cancel.clone()).await;
        }
        // }
    }
}

pub async fn update_state(
    services: &HashMap<ServiceID, Service>,
    service_state: &mut HashMap<ServiceID, State>,
    event: &Event,
) {
    // Update our local status
    match &event {
        Event::Started(service_id) => {
            let service = &services[service_id];
            let health = if service.health_check.is_some() {
                Some(Health::Unhealthy)
            } else {
                None
            };
            service_state.insert(
                service_id.clone(),
                State::Running {
                    // process,
                    health,
                },
            );
        }
        Event::Killed(service_id) => {
            service_state.insert(service_id.clone(), State::Killed {});
        }
        Event::Exited(service_id, code) => {
            service_state.insert(
                service_id.clone(),
                State::Exited {
                    exit_code: *code,
                    restart_policy: services[service_id].restart_policy.clone(),
                },
            );
        }
        Event::Disabled(service_id) => {
            service_state.insert(service_id.clone(), State::Disabled);
        }
        Event::Healthy(service_id) => {
            if let Some(State::Running { health, .. }) = service_state.get_mut(service_id.as_str())
            {
                *health = Some(Health::Healthy);
            }
            // service_state.insert(service_id.clone(), State::Healthy);
        }
        Event::Unhealthy(service_id) => {
            if let Some(State::Running { health, .. }) = service_state.get_mut(service_id.as_str())
            {
                *health = Some(Health::Unhealthy);
            }
            // service_state.insert(service_id.clone(), State::Unhealthy);
        }
    }
}

pub async fn scheduler(
    services: &HashMap<ServiceID, Service>,
    mut events_rx: mpsc::Receiver<Event>,
    mut events_tx: mpsc::Sender<Event>,
    mut broadcast_tx: broadcast::Sender<Event>,
    // mut shutdown_handle: crate::shutdown::Handle,
    cancel: CancellationToken,
) -> eyre::Result<()> {
    let graph = ServiceGraph::new(&services)?;

    // Initially, all services are in pending state
    let mut service_state: HashMap<ServiceID, State> = services
        .keys()
        .map(|service_id| (service_id.to_string(), State::Pending))
        .collect();

    // Initial scheduling pass
    tracing::trace!("started initial scheduling pass");
    schedule_ready(
        &services,
        &graph.inner,
        &mut service_state,
        &events_tx,
        &broadcast_tx,
        &cancel,
        // &shutdown_handle,
    )
    .await;
    tracing::trace!("completed initial scheduling pass");

    // Whenever an event comes in, try to (re)start any services whose deps are now healthy
    // while let Some(event) = events_rx.recv().await {
    let mut rounds_left: usize = 3;
    loop {
        // let shutdown_fut = shutdown_handle.changed();
        tracing::trace!("waiting for scheduling event");
        if rounds_left <= 0 {
            break;
        }
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::trace!("exiting scheduler");
                break;
            }
            event = events_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                tracing::debug!(?event, "received event");

                update_state(services, &mut service_state, &event);

                // Re-broadcast it for anyone else (e.g. UI)
                let _ = broadcast_tx.send(event.clone());

                schedule_ready(
                    &services,
                    &graph.inner,
                    &mut service_state,
                    &events_tx,
                    &broadcast_tx,
                    &cancel,
                )
                .await;
            }
        }
        rounds_left = rounds_left.saturating_sub(1);
    }
    Ok(())
}
