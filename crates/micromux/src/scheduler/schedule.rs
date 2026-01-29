use super::{pty, Event, ServiceID, State};
use crate::{ServiceMap, health_check::Health};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(super) struct ScheduleContext<'a> {
    pub(super) services: &'a ServiceMap,
    pub(super) graph: &'a petgraph::graphmap::DiGraphMap<&'a str, ()>,
    pub(super) service_state: &'a mut HashMap<ServiceID, State>,
    pub(super) desired_disabled: &'a HashSet<ServiceID>,
    pub(super) restart_requested: &'a mut HashSet<ServiceID>,
    pub(super) restart_on_failure_remaining: &'a mut HashMap<ServiceID, usize>,
    pub(super) terminate_tokens: &'a mut HashMap<ServiceID, CancellationToken>,
    pub(super) pty_masters:
        &'a mut HashMap<ServiceID, Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>>,
    pub(super) pty_writers: &'a mut HashMap<ServiceID, Arc<Mutex<Box<dyn Write + Send>>>>,
    pub(super) current_pty_size: portable_pty::PtySize,
    pub(super) restart_backoff_until: &'a HashMap<ServiceID, tokio::time::Instant>,
    pub(super) interactive_logs: bool,
    pub(super) events_tx: &'a mpsc::Sender<Event>,
    pub(super) shutdown: &'a CancellationToken,
}

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

fn dependencies_ready(
    ctx: &ScheduleContext<'_>,
    service_id: &str,
    service: &crate::service::Service,
) -> bool {
    use crate::config::DependencyCondition;

    ctx.graph
        .neighbors_directed(service_id, petgraph::Incoming)
        .all(|dep| {
            let condition = service
                .depends_on
                .iter()
                .find(|dep_config| dep_config.name.as_ref() == dep)
                .and_then(|dep_config| dep_config.condition.as_ref())
                .map(std::convert::AsRef::as_ref)
                .copied()
                .unwrap_or_default();
            let Some(state) = ctx.service_state.get(dep) else {
                return false;
            };

            match condition {
                DependencyCondition::Started => matches!(state, State::Running { .. }),
                DependencyCondition::Healthy => matches!(
                    state,
                    State::Running {
                        health: Some(Health::Healthy),
                        ..
                    }
                ),
                DependencyCondition::CompletedSuccessfully => {
                    matches!(state, State::Exited { exit_code: 0, .. })
                }
            }
        })
}

enum StartCheck {
    Skip,
    Consider { exited_code: Option<i32> },
}

fn should_consider_start(
    ctx: &mut ScheduleContext<'_>,
    service_id: &ServiceID,
    service: &crate::service::Service,
) -> StartCheck {
    use crate::service::RestartPolicy;

    if ctx.desired_disabled.contains(service_id) {
        return StartCheck::Skip;
    }

    if !ctx.restart_requested.contains(service_id)
        && let Some(until) = ctx.restart_backoff_until.get(service_id)
        && tokio::time::Instant::now() < *until
    {
        return StartCheck::Skip;
    }

    let Some(state) = ctx.service_state.get(service_id.as_str()) else {
        return StartCheck::Skip;
    };
    match state {
        State::Pending => StartCheck::Consider { exited_code: None },
        State::Starting | State::Running { .. } | State::Killed | State::Disabled => StartCheck::Skip,
        State::Exited { exit_code } => {
            if ctx.restart_requested.contains(service_id) {
                StartCheck::Consider {
                    exited_code: Some(*exit_code),
                }
            } else {
                match &service.restart_policy {
                    RestartPolicy::Never => StartCheck::Skip,
                    RestartPolicy::Always | RestartPolicy::UnlessStopped => StartCheck::Consider {
                        exited_code: Some(*exit_code),
                    },
                    RestartPolicy::OnFailure { remaining_attempts } => {
                        if *exit_code == 0 {
                            StartCheck::Skip
                        } else {
                            let remaining = ctx
                                .restart_on_failure_remaining
                                .entry(service_id.clone())
                                .or_insert(*remaining_attempts);
                            if *remaining > 0 {
                                StartCheck::Consider {
                                    exited_code: Some(*exit_code),
                                }
                            } else {
                                StartCheck::Skip
                            }
                        }
                    }
                }
            }
        }
    }
}

fn apply_on_failure_decrement(
    ctx: &mut ScheduleContext<'_>,
    service_id: &ServiceID,
    service: &crate::service::Service,
    exited_code: Option<i32>,
) {
    use crate::service::RestartPolicy;

    let Some(exit_code) = exited_code else {
        return;
    };

    if matches!(service.restart_policy, RestartPolicy::OnFailure { .. })
        && !ctx.restart_requested.contains(service_id)
        && exit_code != 0
        && let Some(remaining) = ctx.restart_on_failure_remaining.get_mut(service_id)
    {
        *remaining = remaining.saturating_sub(1);
    }
}

async fn start_service_if_ready(
    ctx: &mut ScheduleContext<'_>,
    service_id: &ServiceID,
    service: &crate::service::Service,
    exited_code: Option<i32>,
) {
    if !dependencies_ready(ctx, service_id.as_str(), service) {
        return;
    }

    tracing::info!(service_id, "starting service");
    apply_on_failure_decrement(ctx, service_id, service, exited_code);

    ctx.restart_requested.remove(service_id);

    if let Some(state) = ctx.service_state.get_mut(service_id.as_str()) {
        *state = State::Starting;
    }

    let terminate = CancellationToken::new();
    ctx.terminate_tokens
        .insert(service_id.clone(), terminate.clone());

    match pty::start_service_with_pty_size(
        service,
        ctx.events_tx.clone(),
        ctx.shutdown.clone(),
        terminate,
        ctx.current_pty_size,
        ctx.interactive_logs,
    )
    .await
    {
        Ok(handles) => {
            ctx.pty_masters.insert(service_id.clone(), handles.master);
            ctx.pty_writers.insert(service_id.clone(), handles.writer);
        }
        Err(err) => {
            tracing::error!(?err, service_id, "failed to start service");
            if let Some(state) = ctx.service_state.get_mut(service_id.as_str()) {
                *state = State::Exited { exit_code: -1 };
            }
        }
    }
}

pub(super) async fn schedule_ready(ctx: &mut ScheduleContext<'_>) {
    for (service_id, service) in ctx.services {
        let exited_code = match should_consider_start(ctx, service_id, service) {
            StartCheck::Skip => continue,
            StartCheck::Consider { exited_code } => exited_code,
        };

        tracing::debug!(
            service_id,
            state = ?ctx.service_state.get(service_id.as_str()),
            "evaluating service"
        );

        start_service_if_ready(ctx, service_id, service, exited_code).await;
    }
}
