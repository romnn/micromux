use super::{
    DesiredState, ProcessEvent, RunConfig, RunningService, ServiceID, ServiceRuntime,
    SessionModelWriter, State, pty, sync_model,
};
#[cfg(test)]
use super::{Event, TestEventSink};
use crate::{ServiceMap, health_check::Health};
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

fn dependencies_ready(ctx: &ScheduleContext<'_>, service: &crate::service::Service) -> bool {
    use crate::config::DependencyCondition;

    service.depends_on.iter().all(|dep| {
        let condition = dep
            .condition
            .as_ref()
            .map(std::convert::AsRef::as_ref)
            .copied()
            .unwrap_or_default();
        let Some(runtime) = ctx.runtimes.get(dep.name.as_ref().as_str()) else {
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
                || runtime.will_auto_restart(&service.restart_policy, exit_code)
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
            service.restart_policy,
            crate::service::RestartPolicy::OnFailure { .. }
        )
    {
        runtime
            .restart
            .decrement_failure_restart(&service.restart_policy);
    }
}

fn start_service_if_ready(
    ctx: &mut ScheduleContext<'_>,
    service_id: &ServiceID,
    service: &crate::service::Service,
    exited_code: Option<i32>,
) {
    if !dependencies_ready(ctx, service) {
        return;
    }

    tracing::info!(service_id, "starting service");

    let Some(runtime) = ctx.runtimes.get_mut(service_id) else {
        return;
    };
    let explicit_start = runtime.start_requested;
    let clear_logs = runtime.clear_logs_on_start;
    decrement_failure_budget(runtime, service, explicit_start, exited_code);

    runtime.start_requested = false;
    runtime.clear_logs_on_start = false;
    // Once the new run is current, a previous run's still-draining output would be dropped as
    // stale anyway, so cancel those readers now instead of letting a descendant that keeps the
    // PTY slave open pin them (and their handles) indefinitely.
    for (_, log_reader) in &mut runtime.draining_log_readers {
        log_reader.cancel();
    }
    runtime.mark_starting();
    runtime.run_config = Some(RunConfig {
        command: service.argv(),
        working_dir: service.working_dir_display(),
        advertised_ports: service.advertised_ports.clone(),
        healthcheck_configured: service.health_check.is_some(),
    });
    sync_model(ctx.writer, service, runtime);

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
        Ok(started) => {
            runtime.mark_started(RunningService {
                run_id,
                terminate,
                log_reader: Some(started.log_reader),
                pty: started.handles,
                since: tokio::time::Instant::now(),
            });
            sync_model(ctx.writer, service, runtime);
            // Model: clear on restart, and always reset the live-snapshot target so the new run's
            // first frame appends rather than replacing the previous run's final frame.
            ctx.writer.begin_run(service_id, run_id.get());
            if clear_logs {
                ctx.writer.clear_logs(service_id);
            }
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
        }
        Err(err) => {
            tracing::error!(?err, service_id, "failed to start service");
            runtime.finish_failed_start(&service.restart_policy, run_id, -1);
            sync_model(ctx.writer, service, runtime);
            #[cfg(test)]
            ctx.test_events
                .forward(Event::Exited(service_id.clone(), -1));
        }
    }
}

pub(super) fn schedule_ready(ctx: &mut ScheduleContext<'_>) {
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

        start_service_if_ready(ctx, service_id, service, exited_code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Dependency, DependencyCondition},
        model::SessionModelWriter,
        service::Service,
    };
    use std::collections::HashMap;
    use std::path::Path;
    use tokio::sync::mpsc;
    use yaml_spanned::Spanned;

    fn spanned_string(value: &str) -> Spanned<String> {
        Spanned {
            span: yaml_spanned::spanned::Span::default(),
            inner: value.to_string(),
        }
    }

    fn test_service(id: &str) -> color_eyre::Result<Service> {
        let cfg = crate::config::Service {
            name: spanned_string(id),
            command: (spanned_string("true"), Vec::new()),
            working_dir: None,
            env_file: Vec::new(),
            environment: indexmap::IndexMap::new(),
            depends_on: Vec::new(),
            healthcheck: None,
            ports: Vec::new(),
            restart: None,
            restart_policy: crate::service::RestartPolicy::Never,
            color: None,
            log_retention: crate::LogRetention::default(),
        };
        Service::new(id, Path::new("."), cfg)
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
    fn dependencies_ready_honors_each_duplicate_entry_condition() -> color_eyre::Result<()> {
        let dep = test_service("dep")?;
        let mut svc = test_service("svc")?;
        svc.depends_on = vec![
            dependency("dep", DependencyCondition::Started),
            dependency("dep", DependencyCondition::Healthy),
        ];

        let mut services = ServiceMap::new();
        services.insert("dep".to_string(), dep.clone());
        services.insert("svc".to_string(), svc.clone());

        let mut runtimes = HashMap::new();
        let mut runtime = ServiceRuntime::new(&dep.restart_policy);
        runtime.state = State::Running { health: None };
        runtimes.insert("dep".to_string(), runtime);
        runtimes.insert("svc".to_string(), ServiceRuntime::new(&svc.restart_policy));

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

        assert!(!dependencies_ready(&ctx, &svc));

        if let Some(runtime) = ctx.runtimes.get_mut("dep") {
            runtime.state = State::Running {
                health: Some(Health::Healthy),
            };
        }
        assert!(dependencies_ready(&ctx, &svc));
        Ok(())
    }
}
