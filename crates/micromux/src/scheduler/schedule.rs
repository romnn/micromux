use super::{pty, Event, ServiceID, State};
use crate::{ServiceMap, health_check::Health};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(super) fn update_state(
    services: &ServiceMap,
    service_state: &mut HashMap<ServiceID, State>,
    event: &Event,
) {
    match event {
        Event::Started { service_id } => {
            service_state.insert(service_id.clone(), State::Running { health: None });
        }
        Event::Healthy(service_id) => {
            service_state.insert(
                service_id.clone(),
                State::Running {
                    health: Some(Health::Healthy),
                },
            );
        }
        Event::Unhealthy(service_id) => {
            service_state.insert(
                service_id.clone(),
                State::Running {
                    health: Some(Health::Unhealthy),
                },
            );
        }
        Event::Killed(service_id) => {
            service_state.insert(service_id.clone(), State::Killed);
        }
        Event::Exited(service_id, exit_code) => {
            service_state.insert(
                service_id.clone(),
                State::Exited {
                    exit_code: *exit_code,
                },
            );
        }
        Event::Disabled(service_id) => {
            service_state.insert(service_id.clone(), State::Disabled);
        }
        Event::LogLine { .. }
        | Event::HealthCheckStarted { .. }
        | Event::HealthCheckLogLine { .. }
        | Event::HealthCheckFinished { .. } => {}
    }

    for service_id in services.keys() {
        service_state
            .entry(service_id.clone())
            .or_insert(State::Pending);
    }
}

pub(super) async fn schedule_ready(
    services: &ServiceMap,
    graph: &petgraph::graphmap::DiGraphMap<&str, ()>,
    service_state: &mut HashMap<ServiceID, State>,
    desired_disabled: &HashSet<ServiceID>,
    restart_requested: &mut HashSet<ServiceID>,
    restart_on_failure_remaining: &mut HashMap<ServiceID, usize>,
    terminate_tokens: &mut HashMap<ServiceID, CancellationToken>,
    pty_masters: &mut HashMap<ServiceID, Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>>,
    pty_writers: &mut HashMap<ServiceID, Arc<Mutex<Box<dyn Write + Send>>>>,
    current_pty_size: portable_pty::PtySize,
    restart_backoff_until: &HashMap<ServiceID, tokio::time::Instant>,
    interactive_logs: bool,
    events_tx: &mpsc::Sender<Event>,
    _ui_tx: &mpsc::Sender<Event>,
    shutdown: &CancellationToken,
) {
    use crate::{config::DependencyCondition, service::RestartPolicy};

    for (service_id, service) in services {
        if desired_disabled.contains(service_id) {
            continue;
        }

        if !restart_requested.contains(service_id)
            && let Some(until) = restart_backoff_until.get(service_id)
            && tokio::time::Instant::now() < *until
        {
            continue;
        }

        let (should_consider_start, exited_code) = match service_state.get(service_id.as_str()) {
            None => continue,
            Some(state) => match state {
                State::Pending => (true, None),
                State::Starting | State::Running { .. } | State::Killed | State::Disabled => {
                    (false, None)
                }
                State::Exited { exit_code } => {
                    if restart_requested.contains(service_id) {
                        (true, Some(*exit_code))
                    } else {
                        match &service.restart_policy {
                            RestartPolicy::Never => (false, Some(*exit_code)),
                            RestartPolicy::Always | RestartPolicy::UnlessStopped => {
                                (true, Some(*exit_code))
                            }
                            RestartPolicy::OnFailure { remaining_attempts } => {
                                if *exit_code == 0 {
                                    (false, Some(*exit_code))
                                } else {
                                    let remaining = restart_on_failure_remaining
                                        .entry(service_id.clone())
                                        .or_insert(*remaining_attempts);
                                    (*remaining > 0, Some(*exit_code))
                                }
                            }
                        }
                    }
                }
            },
        };

        if !should_consider_start {
            continue;
        }

        tracing::debug!(
            service_id,
            state = ?service_state.get(service_id.as_str()),
            "evaluating service"
        );

        let mut dependencies = graph.neighbors_directed(service_id, petgraph::Incoming);

        let is_ready = dependencies.all(|dep| {
            let condition = service
                .depends_on
                .iter()
                .find(|dep_config| dep_config.name.as_ref() == dep)
                .and_then(|dep_config| dep_config.condition.as_ref())
                .map(|condition| condition.as_ref())
                .copied()
                .unwrap_or_default();
            let state = match service_state.get(dep) {
                Some(state) => state,
                None => return false,
            };

            match condition {
                DependencyCondition::ServiceStarted => matches!(state, State::Running { .. }),
                DependencyCondition::ServiceHealthy => matches!(
                    state,
                    State::Running {
                        health: Some(Health::Healthy),
                        ..
                    }
                ),
                DependencyCondition::ServiceCompletedSuccessfully => {
                    matches!(state, State::Exited { exit_code: 0, .. })
                }
            }
        });

        if is_ready {
            tracing::info!(service_id, "starting service");
            if let Some(exit_code) = exited_code {
                let policy = &service.restart_policy;
                if matches!(policy, RestartPolicy::OnFailure { .. })
                    && !restart_requested.contains(service_id)
                    && exit_code != 0
                    && let Some(remaining) = restart_on_failure_remaining.get_mut(service_id)
                {
                    *remaining = remaining.saturating_sub(1);
                }
            }

            restart_requested.remove(service_id);

            if let Some(state) = service_state.get_mut(service_id.as_str()) {
                *state = State::Starting;
            }

            let terminate = CancellationToken::new();
            terminate_tokens.insert(service_id.clone(), terminate.clone());
            match pty::start_service_with_pty_size(
                service,
                events_tx.clone(),
                shutdown.clone(),
                terminate.clone(),
                current_pty_size,
                interactive_logs,
            )
            .await
            {
                Ok(handles) => {
                    pty_masters.insert(service_id.clone(), handles.master);
                    pty_writers.insert(service_id.clone(), handles.writer);
                }
                Err(err) => {
                    tracing::error!(?err, service_id, "failed to start service");
                    if let Some(state) = service_state.get_mut(service_id.as_str()) {
                        *state = State::Exited { exit_code: -1 };
                    }
                }
            }
        }
    }
}
