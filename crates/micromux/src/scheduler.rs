use crate::{
    ServiceMap,
    graph::ServiceGraph,
    health_check::Health,
    service::{self, Service},
};
use color_eyre::eyre;
use futures::{FutureExt, SinkExt, channel::oneshot::Cancellation};
use std::collections::HashMap;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

pub type ServiceID = String;

#[derive(
    Debug,
    strum::Display,
    // strum::IntoStaticStr,
)]
pub enum State {
    /// Service has not yet started.
    // #[strum(serialize = "PENDING")]
    Pending,
    /// Service is running.
    // #[strum(serialize = "RUNNING")]
    Running {
        // process: async_process::Child,
        health: Option<Health>,
    },
    /// Service is disabled.
    // #[strum(serialize = "DISABLED")]
    Disabled,
    /// Service exited with code.
    // #[strum(serialize = "EXITED")]
    Exited {
        exit_code: i32,
        restart_policy: service::RestartPolicy,
    },
    /// Service has been killed and is awaiting exit
    // #[strum(serialize = "KILLED")]
    Killed,
}

#[derive(Debug)]
pub enum Event {
    Started {
        service_id: ServiceID,
        stderr: Option<async_process::ChildStderr>,
        stdout: Option<async_process::ChildStdout>,
    },
    Killed(ServiceID),
    Exited(ServiceID, i32),
    Healthy(ServiceID),
    Unhealthy(ServiceID),
    Disabled(ServiceID),
}

impl Event {
    pub fn service_id(&self) -> &ServiceID {
        match self {
            Self::Started { service_id, .. } => service_id,
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
            Self::Killed(service_id) => write!(f, "Killed({service_id})"),
            Self::Exited(service_id, _) => write!(f, "Exited({service_id})"),
            Self::Healthy(service_id) => write!(f, "Healthy({service_id})"),
            Self::Unhealthy(service_id) => write!(f, "Unhealty({service_id})"),
            Self::Disabled(service_id) => write!(f, "Disabled({service_id})"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Command {
    Restart(ServiceID),
    Disable(ServiceID),
}

/// Start service.
async fn start_service(
    service: &Service,
    events_tx: mpsc::Sender<Event>,
    // shutdown: crate::shutdown::Shutdown,
    // mut shutdown_handle: crate::shutdown::Handle,
    shutdown: CancellationToken,
    terminate: CancellationToken,
) -> eyre::Result<()> {
    use async_process::{Command, Stdio};
    use futures::{AsyncBufReadExt, StreamExt};

    let service_id = service.id.clone();

    tracing::info!(service_id, "start service");

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

    // let mut log = BoundedLog::with_limits(1000, 64 * 1024); // 1000 lines, up to 64 KB

    // let  = |reader, log_clone: Arc<Mutex<BoundedLog>>, tx_clone: mpsc::Sender<()>| {
    //     tokio::spawn(async move {
    //         let mut lines = futures::io::BufReader::new(reader).lines();
    //         while let Ok(Some(line)) = lines.next().await {
    //             let mut lg = log_clone.lock().await;
    //             lg.push(line);
    //             // Notify TUI
    //             let _ = tx_clone.send(()).await;
    //         }
    //     });
    // };

    let stderr = process.stderr.take();
    let stdout = process.stdout.take();

    // let mut cmd = async_process::Command::new(&service.command[0]);
    // cmd.args(&service.command[1..]);
    // let mut child = cmd.spawn().expect("spawn failed");

    let terminate = CancellationToken::new();
    let _ = events_tx
        .send(Event::Started {
            service_id: service_id.clone(),
            stdout,
            stderr,
        })
        .await;

    // let process_clone = process.clone();

    // Monitor for exit or shutdown
    tokio::spawn({
        let events_tx = events_tx.clone();
        let service_id = service_id.clone();
        let shutdown = shutdown.clone();
        let terminate = terminate.clone();
        async move {
            let kill = |service_id: ServiceID,
                        mut process: async_process::Child,
                        events_tx: mpsc::Sender<Event>| async move {
                tracing::info!(pid = process.id(), "killing process");
                // Kill the process
                let _ = events_tx.send(Event::Killed(service_id.clone())).await;
                let _ = process.kill();
                // Optionally wait for it to actually exit
                let _ = process.status().await;
                let _ = events_tx.send(Event::Exited(service_id.clone(), -1)).await;
            };

            tokio::select! {
                _ = shutdown.cancelled() => {
                    kill(service_id.clone(), process, events_tx.clone()).await;
                }
                _ = terminate.cancelled() => {
                    kill(service_id.clone(), process, events_tx.clone()).await;
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
                health_check
                    .run_loop(&service_id, events_tx, shutdown, terminate)
                    .await
            }
        });
    }
    Ok(())
}

async fn schedule_ready(
    services: &ServiceMap,
    graph: &petgraph::graphmap::DiGraphMap<&str, ()>,
    service_state: &mut HashMap<ServiceID, State>,
    // events_rx: &mpsc::Receiver<Event>,
    events_tx: &mpsc::Sender<Event>,
    ui_tx: &mpsc::Sender<Event>,
    // broadcast_tx: &broadcast::Sender<Event>,
    // shutdown_handle: &crate::shutdown::Handle,
    shutdown: &CancellationToken,
) {
    use crate::{config::DependencyCondition, service::RestartPolicy};

    // Find services that are ready to start
    for (service_id, service) in services {
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

        tracing::debug!(
            service_id,
            ?state,
            // state = ?service.state,
            "evaluating service"
        );

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
            // Start service
            tracing::info!(service_id, "starting service");
            let terminate = CancellationToken::new();
            if let Err(err) =
                start_service(service, events_tx.clone(), shutdown.clone(), terminate).await
            {
                tracing::error!(?err, service_id, "failed to start service");
            }
        }
    }
}

pub fn update_state(
    services: &ServiceMap,
    service_state: &mut HashMap<ServiceID, State>,
    event: &Event,
) {
    let (service_id, new_state) = match &event {
        Event::Started { service_id, .. } => {
            let service = &services[service_id];
            let health = if service.health_check.is_some() {
                Some(Health::Unhealthy)
            } else {
                None
            };
            let new_state = State::Running {
                // process,
                health,
            };
            (service_id, new_state)
            // service_state.insert(service_id.clone(), new_state);
        }
        Event::Killed(service_id) => {
            let new_state = State::Killed {};
            // tracing::debug!(service_id, ?new_state, "update state");
            // service_state.insert(service_id.clone(), new_state);
            (service_id, new_state)
        }
        Event::Exited(service_id, code) => {
            let new_state = State::Exited {
                exit_code: *code,
                restart_policy: services[service_id].restart_policy.clone(),
            };
            // service_state.insert(service_id.clone(), new_state);
            (service_id, new_state)
        }
        Event::Disabled(service_id) => {
            let new_state = State::Disabled;
            (service_id, new_state)
            // service_state.insert(service_id.clone(), );
        }
        Event::Healthy(service_id) => {
            let new_state = State::Running {
                health: Some(Health::Healthy),
            };
            // if let Some(State::Running { health, .. }) = service_state.get_mut(service_id.as_str())
            // {
            //     *health = Some(Health::Healthy);
            // }
            // service_state.insert(service_id.clone(), State::Healthy);
            (service_id, new_state)
        }
        Event::Unhealthy(service_id) => {
            // if let Some(State::Running { health, .. }) = service_state.get_mut(service_id.as_str())
            // {
            //     *health = Some(Health::Unhealthy);
            // }
            let new_state = State::Running {
                health: Some(Health::Unhealthy),
            };
            (service_id, new_state)
            // service_state.insert(service_id.clone(), State::Unhealthy);
        }
    };

    // if let Some((service_id, new_state)) = new_state {
    tracing::debug!(service_id, ?new_state, "update state");
    service_state.insert(service_id.clone(), new_state);
}

pub async fn scheduler(
    services: &ServiceMap,
    mut commands_rx: mpsc::Receiver<Command>,
    mut events_rx: mpsc::Receiver<Event>,
    mut events_tx: mpsc::Sender<Event>,
    mut ui_tx: mpsc::Sender<Event>,
    // mut broadcast_tx: broadcast::Sender<Event>,
    // mut shutdown_handle: crate::shutdown::Handle,
    shutdown: CancellationToken,
) -> eyre::Result<()> {
    let graph = ServiceGraph::new(&services)?;

    // Initially, all services are in pending state
    let mut service_state: HashMap<ServiceID, State> = services
        .keys()
        .map(|service_id| (service_id.to_string(), State::Pending))
        .collect();

    // Initial scheduling pass
    tracing::debug!("started initial scheduling pass");
    schedule_ready(
        &services,
        &graph.inner,
        &mut service_state,
        &events_tx,
        &ui_tx,
        &shutdown,
    )
    .await;
    tracing::debug!("completed initial scheduling pass");

    // Whenever an event comes in, try to (re)start any services whose deps are now healthy
    let mut rounds_left: usize = 3;
    loop {
        tracing::debug!("waiting for scheduling event");
        if rounds_left <= 0 {
            break;
        }
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::debug!("exiting scheduler");
                break;
            }
            command = commands_rx.recv() => {
                let Some(command) = command else {
                    break;
                };
                tracing::debug!(?command, "received command");
                match command {
                    Command::Restart(service_id) => {
                        tracing::debug!(service_id, "TODO: restart service");
                    },
                    Command::Disable(service_id) => {
                        tracing::debug!(service_id, "TODO: disable service");
                    },
                }
            }
            event = events_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                tracing::debug!(%event, "received event");

                update_state(services, &mut service_state, &event);

                // Forward event to the UI
                ui_tx.send(event).await?;
            }
        }

        schedule_ready(
            &services,
            &graph.inner,
            &mut service_state,
            &events_tx,
            &ui_tx,
            &shutdown,
        )
        .await;
        rounds_left = rounds_left.saturating_sub(1);
    }
    Ok(())
}
