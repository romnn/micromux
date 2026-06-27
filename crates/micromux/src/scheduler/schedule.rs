use super::{
    DesiredState, Event, ProcessEvent, RunningService, ServiceID, ServiceRuntime, State, pty,
};
use crate::{ServiceMap, health_check::Health};
use color_eyre::eyre;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(super) struct ScheduleContext<'a> {
    pub(super) services: &'a ServiceMap,
    pub(super) graph: &'a petgraph::graphmap::DiGraphMap<&'a str, ()>,
    pub(super) runtimes: &'a mut HashMap<ServiceID, ServiceRuntime>,
    pub(super) current_pty_size: portable_pty::PtySize,
    pub(super) events_tx: &'a mpsc::Sender<ProcessEvent>,
    pub(super) ui_tx: &'a mpsc::Sender<Event>,
    pub(super) shutdown: &'a CancellationToken,
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
            let Some(runtime) = ctx.runtimes.get(dep) else {
                return false;
            };

            if runtime.desired == DesiredState::Disabled || runtime.start_requested {
                return false;
            }

            match condition {
                DependencyCondition::Started => matches!(runtime.state, State::Running { .. }),
                DependencyCondition::Healthy => matches!(
                    runtime.state,
                    State::Running {
                        health: Some(Health::Healthy),
                        ..
                    }
                ),
                DependencyCondition::CompletedSuccessfully => {
                    matches!(runtime.state, State::Exited { exit_code: 0, .. })
                }
            }
        })
}

enum StartCheck {
    Skip,
    Consider { exited_code: Option<i32> },
}

fn should_consider_start(
    ctx: &ScheduleContext<'_>,
    service_id: &ServiceID,
    service: &crate::service::Service,
) -> StartCheck {
    use crate::service::RestartPolicy;

    let Some(runtime) = ctx.runtimes.get(service_id) else {
        return StartCheck::Skip;
    };

    if runtime.desired == DesiredState::Disabled || runtime.running.is_some() {
        return StartCheck::Skip;
    }

    if !runtime.start_requested
        && let Some(until) = runtime.restart.backoff_until
        && tokio::time::Instant::now() < until
    {
        return StartCheck::Skip;
    }

    match runtime.state {
        State::Pending => StartCheck::Consider { exited_code: None },
        State::Starting | State::Running { .. } | State::Killed | State::Disabled => {
            StartCheck::Skip
        }
        State::Exited { exit_code } => {
            if runtime.start_requested {
                StartCheck::Consider {
                    exited_code: Some(exit_code),
                }
            } else {
                match &service.restart_policy {
                    RestartPolicy::Never => StartCheck::Skip,
                    RestartPolicy::Always | RestartPolicy::UnlessStopped => StartCheck::Consider {
                        exited_code: Some(exit_code),
                    },
                    RestartPolicy::OnFailure { max_attempts } => {
                        if exit_code == 0 {
                            StartCheck::Skip
                        } else if max_attempts.is_some() {
                            if runtime
                                .restart
                                .remaining_failure_restarts(&service.restart_policy)
                                .is_some_and(|remaining| remaining > 0)
                            {
                                StartCheck::Consider {
                                    exited_code: Some(exit_code),
                                }
                            } else {
                                StartCheck::Skip
                            }
                        } else {
                            StartCheck::Consider {
                                exited_code: Some(exit_code),
                            }
                        }
                    }
                }
            }
        }
    }
}

fn decrement_failure_budget(
    runtime: &mut ServiceRuntime,
    service: &crate::service::Service,
    explicit_start: bool,
    exited_code: Option<i32>,
) {
    if !explicit_start
        && exited_code.is_some_and(|exit_code| exit_code != 0)
        && matches!(
            service.restart_policy,
            crate::service::RestartPolicy::OnFailure { .. }
        )
    {
        runtime
            .restart
            .decrement_failure_restart(&service.restart_policy);
    }
}

async fn start_service_if_ready(
    ctx: &mut ScheduleContext<'_>,
    service_id: &ServiceID,
    service: &crate::service::Service,
    exited_code: Option<i32>,
) -> eyre::Result<()> {
    if !dependencies_ready(ctx, service_id.as_str(), service) {
        return Ok(());
    }

    tracing::info!(service_id, "starting service");

    let Some(runtime) = ctx.runtimes.get_mut(service_id) else {
        return Ok(());
    };
    let explicit_start = runtime.start_requested;
    let clear_logs = runtime.clear_logs_on_start;
    decrement_failure_budget(runtime, service, explicit_start, exited_code);

    runtime.start_requested = false;
    runtime.clear_logs_on_start = false;
    runtime.state = State::Starting;

    let run_id = runtime.allocate_run_id();
    let terminate = CancellationToken::new();

    match pty::start_service_with_pty_size(
        service,
        run_id,
        ctx.events_tx,
        ctx.shutdown,
        &terminate,
        ctx.current_pty_size,
    ) {
        Ok(pty) => {
            runtime.running = Some(RunningService {
                run_id,
                terminate,
                pty,
                since: tokio::time::Instant::now(),
            });
            runtime.state = State::Running { health: None };
            if clear_logs {
                ctx.ui_tx.send(Event::ClearLogs(service_id.clone())).await?;
            }
            ctx.ui_tx
                .send(Event::Started {
                    service_id: service_id.clone(),
                })
                .await?;
        }
        Err(err) => {
            tracing::error!(?err, service_id, "failed to start service");
            runtime.finish_current_run(&service.restart_policy, -1);
            ctx.ui_tx
                .send(Event::Exited(service_id.clone(), -1))
                .await?;
        }
    }

    Ok(())
}

pub(super) async fn schedule_ready(ctx: &mut ScheduleContext<'_>) -> eyre::Result<()> {
    for (service_id, service) in ctx.services {
        let exited_code = match should_consider_start(ctx, service_id, service) {
            StartCheck::Skip => continue,
            StartCheck::Consider { exited_code } => exited_code,
        };

        tracing::debug!(
            service_id,
            state = ?ctx.runtimes.get(service_id).map(|runtime| &runtime.state),
            "evaluating service"
        );

        start_service_if_ready(ctx, service_id, service, exited_code).await?;
    }

    Ok(())
}
