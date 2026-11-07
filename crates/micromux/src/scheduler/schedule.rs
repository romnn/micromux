use super::{
    DesiredState, ProcessEvent, RunConfig, RunningService, ServiceID, ServiceRuntime,
    SessionModelWriter, State, pty, service_event, sync_model,
};
#[cfg(test)]
use super::{Event, ServiceRuntimeInit, TestEventSink};
use crate::config::DependencyCondition;
use crate::model::{DependencyFailure, DependencyFailureReason};
use crate::{DependencySpec, ServiceEventKind, ServiceMap, health_check::Health};
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(super) struct ScheduleContext<'a> {
    pub(super) services: &'a ServiceMap,
    pub(super) runtimes: &'a mut HashMap<ServiceID, ServiceRuntime>,
    pub(super) current_pty_size: portable_pty::PtySize,
    pub(super) events_tx: &'a mpsc::Sender<ProcessEvent>,
    #[cfg(test)]
    pub(super) test_events: &'a mut TestEventSink,
    pub(super) writer: &'a SessionModelWriter,
    pub(super) shutdown: &'a CancellationToken,
}

/// One dependency that is holding a service back, kept with its condition so the wait can be
/// reported as the operator wrote it ("waiting for `db` to become healthy") rather than as a bare
/// service name.
struct BlockedDependency {
    service: ServiceID,
    condition: DependencyCondition,
}

impl BlockedDependency {
    fn describe(&self) -> String {
        let condition = match self.condition {
            DependencyCondition::Started => "to start",
            DependencyCondition::Healthy => "to become healthy",
            DependencyCondition::CompletedSuccessfully => "to complete successfully",
        };
        format!("{} {condition}", self.service)
    }
}

fn describe_blockers(blocked: &[BlockedDependency]) -> String {
    blocked
        .iter()
        .map(BlockedDependency::describe)
        .collect::<Vec<_>>()
        .join(", ")
}

fn blocking_dependencies(
    ctx: &ScheduleContext<'_>,
    service: &crate::service::Service,
) -> Vec<BlockedDependency> {
    service
        .spec
        .depends_on
        .iter()
        .filter_map(|dep| {
            let condition = dep.condition;
            let blocked = || BlockedDependency {
                service: dep.service.clone(),
                condition,
            };
            let Some(runtime) = ctx.runtimes.get(dep.service.as_str()) else {
                return Some(blocked());
            };

            if runtime.desired == DesiredState::Disabled || runtime.start_requested {
                return Some(blocked());
            }

            let ready = match condition {
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
            };
            (!ready).then(blocked)
        })
        .collect()
}

/// Find a dependency of `service` that cannot meet its condition until someone intervenes.
///
/// Dependencies are checked in declaration order and the first failure wins. A dependency that
/// still has to start but is itself stuck behind such a dependency fails as well, so the result
/// names the root cause and the chain of blocked services leading to it.
pub(super) fn dependency_failure(
    services: &ServiceMap,
    runtimes: &HashMap<ServiceID, ServiceRuntime>,
    service: &crate::service::Service,
) -> Option<DependencyFailure> {
    let mut visiting = vec![service.id.clone()];
    service
        .spec
        .depends_on
        .iter()
        .find_map(|dependency| unmeetable_dependency(services, runtimes, dependency, &mut visiting))
}

fn unmeetable_dependency(
    services: &ServiceMap,
    runtimes: &HashMap<ServiceID, ServiceRuntime>,
    dependency: &DependencySpec,
    visiting: &mut Vec<ServiceID>,
) -> Option<DependencyFailure> {
    let failure = |reason, exit_code| DependencyFailure {
        service: dependency.service.clone(),
        condition: dependency.condition,
        reason,
        exit_code,
        via: Vec::new(),
    };
    let (Some(runtime), Some(service)) = (
        runtimes.get(&dependency.service),
        services.get(&dependency.service),
    ) else {
        return Some(failure(DependencyFailureReason::Missing, None));
    };
    if runtime.is_retired() {
        return Some(failure(
            DependencyFailureReason::Retired,
            runtime.last_exit_code,
        ));
    }
    if runtime.desired == DesiredState::Disabled {
        return Some(failure(
            DependencyFailureReason::Disabled,
            runtime.last_exit_code,
        ));
    }
    // A live run keeps the healthcheck it started with; the configured one applies to the next
    // run, including one a pending restart is about to start.
    let has_healthcheck = match (&runtime.running, &runtime.run_config) {
        (Some(_), Some(run_config)) if !runtime.start_requested => run_config.healthcheck.is_some(),
        _ => service.spec.healthcheck.is_some(),
    };
    if dependency.condition == DependencyCondition::Healthy && !has_healthcheck {
        return Some(failure(
            DependencyFailureReason::NoHealthcheck,
            runtime.last_exit_code,
        ));
    }
    if runtime.running.is_some() {
        return None;
    }
    // A requested start still waits for the dependency's own dependencies.
    match runtime.state {
        _ if runtime.start_requested => {}
        State::Exited { exit_code } => {
            let completed = dependency.condition == DependencyCondition::CompletedSuccessfully
                && exit_code == 0;
            if completed {
                return None;
            }
            if !runtime.will_auto_restart(&service.spec.restart, exit_code) {
                return Some(failure(DependencyFailureReason::Exited, Some(exit_code)));
            }
        }
        State::Pending => {}
        State::Starting | State::Running { .. } | State::Killed | State::Disabled => return None,
    }

    // The dependency still has to start, which it never will while one of its own dependencies
    // is stuck. The graph is acyclic, but a cycle guard costs nothing.
    if visiting.contains(&dependency.service) {
        return None;
    }
    visiting.push(dependency.service.clone());
    let nested = service
        .spec
        .depends_on
        .iter()
        .find_map(|nested| unmeetable_dependency(services, runtimes, nested, visiting));
    visiting.pop();
    nested.map(|mut failure| {
        failure.via.insert(0, dependency.service.clone());
        failure
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
            if runtime.start_requested
                || runtime.will_auto_restart(&service.spec.restart, exit_code)
            {
                StartCheck::Consider {
                    exited_code: Some(exit_code),
                }
            } else {
                StartCheck::Skip
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
            service.spec.restart,
            crate::service::RestartPolicy::OnFailure { .. }
        )
    {
        runtime
            .restart
            .decrement_failure_restart(&service.spec.restart);
    }
}

fn record_dependency_block(
    ctx: &mut ScheduleContext<'_>,
    service_id: &ServiceID,
    service: &crate::service::Service,
    blocked: &[BlockedDependency],
) {
    let blocked_on = blocked
        .iter()
        .map(|dependency| dependency.service.clone())
        .collect::<Vec<_>>();
    let failure = dependency_failure(ctx.services, ctx.runtimes, service);
    let Some(runtime) = ctx.runtimes.get_mut(service_id) else {
        return;
    };
    let blockers_changed = runtime.blocked_on != blocked_on;
    let failure_changed = runtime.dependency_failure != failure;
    if !blockers_changed && !failure_changed {
        return;
    }
    runtime.blocked_on.clone_from(&blocked_on);
    runtime.dependency_failure.clone_from(&failure);
    let generation = runtime.run_generation();
    sync_model(ctx.writer, service, runtime);

    if blockers_changed {
        let mut event = service_event(
            generation,
            ServiceEventKind::DependencyBlocked,
            format!("waiting for {}", describe_blockers(blocked)),
        );
        event.blocked_on = Some(blocked_on.clone());
        ctx.writer.append_event(service_id, event);
    }
    if failure_changed && let Some(failure) = failure {
        let mut event = service_event(
            generation,
            ServiceEventKind::DependencyFailed,
            failure.describe(),
        );
        event.exit_code = failure.exit_code;
        event.blocked_on = Some(blocked_on);
        ctx.writer.append_event(service_id, event);
    }
}

/// Forget a recorded dependency block once the service stops being a start candidate — it was
/// disabled, retired, or parked in a restart backoff. Without this the snapshot would keep
/// reporting `Blocked` for a service nothing is waiting to start.
fn clear_dependency_block(
    ctx: &mut ScheduleContext<'_>,
    service_id: &ServiceID,
    service: &crate::service::Service,
) {
    let Some(runtime) = ctx.runtimes.get_mut(service_id) else {
        return;
    };
    if runtime.blocked_on.is_empty() {
        return;
    }
    runtime.blocked_on.clear();
    runtime.dependency_failure = None;
    sync_model(ctx.writer, service, runtime);
}

fn finish_service_start(
    ctx: &mut ScheduleContext<'_>,
    service_id: &ServiceID,
    service: &crate::service::Service,
    run_id: super::RunId,
    terminate: CancellationToken,
    #[cfg(test)] clear_logs: bool,
    result: Result<pty::StartedPty, pty::Error>,
) -> bool {
    let Some(runtime) = ctx.runtimes.get_mut(service_id) else {
        return false;
    };
    match result {
        Ok(started) => {
            let pid = started.pid;
            runtime.mark_started(RunningService {
                run_id,
                pid,
                terminate,
                log_reader: Some(started.log_reader),
                pty: started.handles,
                since: tokio::time::Instant::now(),
                exited_at: None,
                environment: service.spec.environment.clone(),
            });
            sync_model(ctx.writer, service, runtime);
            let mut event = service_event(
                run_id.get(),
                ServiceEventKind::Spawned,
                pid.map_or_else(
                    || "spawned service process".to_string(),
                    |pid| format!("spawned service process with pid {pid}"),
                ),
            );
            event.pid = pid;
            ctx.writer.append_event(service_id, event);
            #[cfg(test)]
            {
                if clear_logs {
                    ctx.test_events
                        .forward(Event::ClearLogs(service_id.clone()));
                }
                ctx.test_events.forward(Event::Started {
                    service_id: service_id.clone(),
                });
            }
            true
        }
        Err(err) => {
            tracing::error!(?err, service_id, "failed to start service");
            runtime.finish_failed_start(&service.spec.restart, run_id, -1);
            sync_model(ctx.writer, service, runtime);
            ctx.writer.append_event(
                service_id,
                service_event(
                    run_id.get(),
                    ServiceEventKind::SpawnFailed,
                    format!("failed to spawn service: {err}"),
                ),
            );
            let mut exited = service_event(
                run_id.get(),
                ServiceEventKind::Exited,
                "service spawn failed",
            );
            exited.exit_code = Some(-1);
            ctx.writer.append_event(service_id, exited);
            if let Some(delay) = runtime.restart.backoff_delay {
                let mut backoff = service_event(
                    run_id.get(),
                    ServiceEventKind::BackoffScheduled,
                    format!("automatic restart scheduled after {} ms", delay.as_millis()),
                );
                backoff.delay_ms = u64::try_from(delay.as_millis()).ok();
                ctx.writer.append_event(service_id, backoff);
            }
            #[cfg(test)]
            ctx.test_events
                .forward(Event::Exited(service_id.clone(), -1));
            false
        }
    }
}

fn start_service_if_ready(
    ctx: &mut ScheduleContext<'_>,
    service_id: &ServiceID,
    service: &crate::service::Service,
    exited_code: Option<i32>,
) -> bool {
    if !blocking_dependencies(ctx, service).is_empty() {
        return false;
    }

    tracing::info!(service_id, "starting service");

    let Some(runtime) = ctx.runtimes.get_mut(service_id) else {
        return false;
    };
    if !runtime.blocked_on.is_empty() {
        runtime.blocked_on.clear();
        runtime.dependency_failure = None;
        ctx.writer.append_event(
            service_id,
            service_event(
                runtime.run_generation(),
                ServiceEventKind::DependencyReady,
                "dependencies are ready",
            ),
        );
    }
    let explicit_start = runtime.start_requested;
    let clear_logs = runtime.clear_logs_on_start;
    decrement_failure_budget(runtime, service, explicit_start, exited_code);

    runtime.start_requested = false;
    runtime.clear_logs_on_start = false;
    // Bound the old run's tail capture before starting its replacement. The entry remains until
    // acknowledgement so a late completion cannot alias a reused run ID; non-Unix readers still
    // rely on EOF because their PTY reads are not cancellable.
    for draining in &mut runtime.draining_log_readers {
        draining.request_cancel();
    }
    runtime.mark_starting();
    runtime.run_config = Some(RunConfig::from(service));
    sync_model(ctx.writer, service, runtime);

    let run_id = runtime.allocate_run_id();
    let terminate = CancellationToken::new();
    ctx.writer.begin_run(service_id, run_id.get());
    if clear_logs {
        ctx.writer.clear_logs(service_id);
    }
    let sink = ctx.writer.run_sink(service_id, run_id.get());

    let result = pty::start_service_with_pty_size(pty::StartServiceParams {
        service,
        run_id,
        sink,
        events_tx: ctx.events_tx,
        shutdown: ctx.shutdown,
        terminate: &terminate,
        pty_size: ctx.current_pty_size,
    });
    finish_service_start(
        ctx,
        service_id,
        service,
        run_id,
        terminate,
        #[cfg(test)]
        clear_logs,
        result,
    )
}

/// Records only blockers that remain after every startable dependency has been started.
///
/// Recording during an intermediate fixed-point pass would expose declaration order as a
/// short-lived `Blocked` state even when all dependencies start in the same scheduling operation.
fn record_stable_dependency_blocks(ctx: &mut ScheduleContext<'_>) {
    for (service_id, service) in ctx.services {
        let StartCheck::Consider { .. } = should_consider_start(ctx, service_id, service) else {
            continue;
        };
        let blocked = blocking_dependencies(ctx, service);
        if !blocked.is_empty() {
            record_dependency_block(ctx, service_id, service, &blocked);
        }
    }
}

pub(super) fn schedule_ready(ctx: &mut ScheduleContext<'_>) {
    loop {
        let mut started = false;
        for (service_id, service) in ctx.services {
            let exited_code = match should_consider_start(ctx, service_id, service) {
                StartCheck::Skip => {
                    clear_dependency_block(ctx, service_id, service);
                    continue;
                }
                StartCheck::Consider { exited_code } => exited_code,
            };

            tracing::debug!(
                service_id,
                state = ?ctx.runtimes.get(service_id).map(|runtime| &runtime.state),
                "evaluating service"
            );

            started |= start_service_if_ready(ctx, service_id, service, exited_code);
        }
        // A successful start is the only synchronous transition that can unblock an earlier
        // candidate. Health and exit changes arrive later as events, so successful starts alone
        // participate in this bounded fixed-point pass.
        if !started {
            record_stable_dependency_blocks(ctx);
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Dependency, DependencyCondition},
        model::SessionModelWriter,
        service::Service,
        test_util::{service_config, spanned_string},
    };
    use color_eyre::eyre;
    use similar_asserts::assert_eq;
    use std::collections::HashMap;
    use std::path::Path;
    use tokio::sync::mpsc;
    use yaml_spanned::Spanned;

    fn test_service(id: &str) -> eyre::Result<Service> {
        let cfg = service_config(id, ("true", &[]));
        Ok(Service::new(id, Path::new("."), cfg)?)
    }

    fn dependency(name: &str, condition: DependencyCondition) -> Dependency {
        Dependency {
            name: spanned_string(name),
            condition: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: condition,
            }),
        }
    }

    fn test_context<'a>(
        services: &'a ServiceMap,
        runtimes: &'a mut HashMap<ServiceID, ServiceRuntime>,
        events_tx: &'a mpsc::Sender<ProcessEvent>,
        writer: &'a SessionModelWriter,
        shutdown: &'a CancellationToken,
        #[cfg(test)] test_events: &'a mut TestEventSink,
    ) -> ScheduleContext<'a> {
        ScheduleContext {
            services,
            runtimes,
            current_pty_size: portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            events_tx,
            #[cfg(test)]
            test_events,
            writer,
            shutdown,
        }
    }

    #[test]
    fn dependencies_ready_honors_each_duplicate_entry_condition() -> eyre::Result<()> {
        let dep = test_service("dep")?;
        let mut svc = test_service("svc")?;
        svc.spec.depends_on = vec![
            dependency("dep", DependencyCondition::Started),
            dependency("dep", DependencyCondition::Healthy),
        ]
        .into_iter()
        .map(|dependency| crate::DependencySpec {
            service: dependency.name.into_inner(),
            condition: dependency
                .condition
                .map(Spanned::into_inner)
                .unwrap_or_default(),
        })
        .collect();

        let mut services = ServiceMap::new();
        services.insert("dep".to_string(), dep.clone());
        services.insert("svc".to_string(), svc.clone());

        let mut runtimes = HashMap::new();
        let mut runtime = ServiceRuntime::new(ServiceRuntimeInit::from(&dep));
        runtime.state = State::Running { health: None };
        runtimes.insert("dep".to_string(), runtime);
        runtimes.insert(
            "svc".to_string(),
            ServiceRuntime::new(ServiceRuntimeInit::from(&svc)),
        );

        let (events_tx, _events_rx) = mpsc::channel(1);
        let (_reader, writer) = crate::model::new([]);
        let shutdown = CancellationToken::new();
        let (test_tx, _test_rx) = mpsc::channel(1);
        let mut test_events = TestEventSink::new(test_tx);
        let ctx = test_context(
            &services,
            &mut runtimes,
            &events_tx,
            &writer,
            &shutdown,
            &mut test_events,
        );

        // Only the `healthy` entry blocks: `dep` is running, so its `started` entry is satisfied.
        assert_eq!(
            describe_blockers(&blocking_dependencies(&ctx, &svc)),
            "dep to become healthy"
        );

        if let Some(runtime) = ctx.runtimes.get_mut("dep") {
            runtime.state = State::Running {
                health: Some(Health::Healthy),
            };
        }
        assert!(blocking_dependencies(&ctx, &svc).is_empty());
        Ok(())
    }
}
