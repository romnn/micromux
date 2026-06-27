use crate::{
    ServiceMap,
    graph::ServiceGraph,
    service::{self},
};
use color_eyre::eyre;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Initial delay before automatically restarting a service after it exits.
const RESTART_BACKOFF_BASE: Duration = Duration::from_millis(250);
/// Maximum delay the (exponentially doubling) restart backoff grows to.
const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(10);
/// Minimum uptime after which a service is considered stable and its backoff is reset.
const RESTART_BACKOFF_RESET: Duration = RESTART_BACKOFF_MAX;

#[path = "scheduler/types.rs"]
mod types;
pub use types::{Command, Event, LogUpdateKind, OutputStream, ServiceID, State};
pub(crate) use types::{ProcessEvent, RunId};

#[path = "scheduler/pty.rs"]
mod pty;

#[path = "scheduler/schedule.rs"]
mod schedule;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DesiredState {
    Enabled,
    Disabled,
}

struct RestartTracker {
    backoff_until: Option<tokio::time::Instant>,
    backoff_delay: Option<Duration>,
    on_failure_max: Option<usize>,
    on_failure_remaining: Option<usize>,
}

impl RestartTracker {
    fn new(policy: &service::RestartPolicy) -> Self {
        let on_failure_max = match policy {
            service::RestartPolicy::OnFailure {
                max_attempts: Some(max_attempts),
            } => Some(*max_attempts),
            service::RestartPolicy::Always
            | service::RestartPolicy::UnlessStopped
            | service::RestartPolicy::Never
            | service::RestartPolicy::OnFailure { max_attempts: None } => None,
        };

        Self {
            backoff_until: None,
            backoff_delay: None,
            on_failure_max,
            on_failure_remaining: on_failure_max,
        }
    }

    fn clear_backoff(&mut self) {
        self.backoff_until = None;
        self.backoff_delay = None;
    }

    fn reset_failure_budget(&mut self) {
        self.on_failure_remaining = self.on_failure_max;
    }

    fn apply_backoff(&mut self, stable: bool) {
        if stable {
            self.backoff_delay = None;
            self.reset_failure_budget();
        }

        let next = self.backoff_delay.map_or(RESTART_BACKOFF_BASE, |delay| {
            (delay * 2).min(RESTART_BACKOFF_MAX)
        });
        self.backoff_delay = Some(next);
        self.backoff_until = Some(tokio::time::Instant::now() + next);
    }

    fn remaining_failure_restarts(&self, policy: &service::RestartPolicy) -> Option<usize> {
        match policy {
            service::RestartPolicy::OnFailure {
                max_attempts: Some(max_attempts),
            } => Some(self.on_failure_remaining.unwrap_or(*max_attempts)),
            service::RestartPolicy::Always
            | service::RestartPolicy::UnlessStopped
            | service::RestartPolicy::Never
            | service::RestartPolicy::OnFailure { max_attempts: None } => None,
        }
    }

    fn decrement_failure_restart(&mut self, policy: &service::RestartPolicy) {
        if let service::RestartPolicy::OnFailure {
            max_attempts: Some(max_attempts),
        } = policy
        {
            let remaining = self.on_failure_remaining.get_or_insert(*max_attempts);
            *remaining = remaining.saturating_sub(1);
        }
    }
}

pub(super) struct RunningService {
    run_id: RunId,
    terminate: CancellationToken,
    log_reader: pty::LogReaderHandle,
    pty: pty::PtyHandles,
    since: tokio::time::Instant,
}

impl RunningService {
    fn cancel(&self) {
        self.terminate.cancel();
    }

    fn stable(&self) -> bool {
        self.since.elapsed() >= RESTART_BACKOFF_RESET
    }
}

impl Drop for RunningService {
    fn drop(&mut self) {
        self.terminate.cancel();
        self.log_reader.cancel();
    }
}

pub(super) struct ServiceRuntime {
    desired: DesiredState,
    start_requested: bool,
    clear_logs_on_start: bool,
    next_run_id: u64,
    running: Option<RunningService>,
    restart: RestartTracker,
    state: State,
}

impl ServiceRuntime {
    fn new(policy: &service::RestartPolicy) -> Self {
        Self {
            desired: DesiredState::Enabled,
            start_requested: false,
            clear_logs_on_start: false,
            next_run_id: 0,
            running: None,
            restart: RestartTracker::new(policy),
            state: State::Pending,
        }
    }

    fn current_run_id(&self) -> Option<RunId> {
        self.running.as_ref().map(|running| running.run_id)
    }

    fn allocate_run_id(&mut self) -> RunId {
        self.next_run_id = self.next_run_id.checked_add(1).unwrap_or(1);
        RunId::new(self.next_run_id)
    }

    fn request_restart(&mut self) {
        self.desired = DesiredState::Enabled;
        self.start_requested = true;
        self.clear_logs_on_start = true;
        self.restart.clear_backoff();
        self.restart.reset_failure_budget();
        if matches!(self.state, State::Disabled) && self.running.is_none() {
            self.state = State::Pending;
        }
        if let Some(running) = &self.running {
            running.cancel();
            self.state = State::Killed;
        }
    }

    fn request_enable(&mut self) {
        self.desired = DesiredState::Enabled;
        self.restart.clear_backoff();
        self.restart.reset_failure_budget();

        if self.running.is_some() && !matches!(self.state, State::Disabled | State::Killed) {
            return;
        }

        self.start_requested = true;
        self.clear_logs_on_start = false;
        if matches!(self.state, State::Disabled) && self.running.is_none() {
            self.state = State::Pending;
        }
        if let Some(running) = &self.running {
            running.cancel();
            self.state = State::Killed;
        }
    }

    fn disable(&mut self) {
        self.desired = DesiredState::Disabled;
        self.start_requested = false;
        self.clear_logs_on_start = false;
        self.restart.clear_backoff();
        self.state = State::Disabled;
        if let Some(running) = &self.running {
            running.cancel();
        }
    }

    fn finish_current_run(&mut self, policy: &service::RestartPolicy, exit_code: i32) {
        let stable = self.running.as_ref().is_some_and(RunningService::stable);
        self.running.take();
        if self.desired == DesiredState::Disabled {
            self.state = State::Disabled;
            self.restart.clear_backoff();
        } else {
            self.state = State::Exited { exit_code };
            if self.start_requested {
                self.restart.clear_backoff();
            } else if self.will_auto_restart(policy, exit_code) {
                self.restart.apply_backoff(stable);
            } else {
                self.restart.clear_backoff();
            }
        }
    }

    fn will_auto_restart(&self, policy: &service::RestartPolicy, exit_code: i32) -> bool {
        match policy {
            service::RestartPolicy::Always | service::RestartPolicy::UnlessStopped => true,
            service::RestartPolicy::Never => false,
            service::RestartPolicy::OnFailure { max_attempts } => {
                if exit_code == 0 {
                    false
                } else if max_attempts.is_none() {
                    true
                } else {
                    self.restart
                        .remaining_failure_restarts(policy)
                        .is_some_and(|remaining| remaining > 0)
                }
            }
        }
    }
}

struct SchedulerRuntime<'a> {
    graph: ServiceGraph<'a>,
    services: HashMap<ServiceID, ServiceRuntime>,
    current_pty_size: portable_pty::PtySize,
    events_tx: mpsc::Sender<ProcessEvent>,
    ui_tx: mpsc::Sender<Event>,
    shutdown: CancellationToken,
}

impl<'a> SchedulerRuntime<'a> {
    fn new(
        services: &ServiceMap,
        graph: ServiceGraph<'a>,
        events_tx: mpsc::Sender<ProcessEvent>,
        ui_tx: mpsc::Sender<Event>,
        shutdown: CancellationToken,
    ) -> Self {
        let services = services
            .iter()
            .map(|(service_id, service)| {
                (
                    service_id.clone(),
                    ServiceRuntime::new(&service.restart_policy),
                )
            })
            .collect();

        Self {
            graph,
            services,
            current_pty_size: portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            events_tx,
            ui_tx,
            shutdown,
        }
    }

    async fn schedule_pass(&mut self, services: &ServiceMap) -> eyre::Result<()> {
        schedule::schedule_ready(&mut schedule::ScheduleContext {
            services,
            graph: &self.graph.inner,
            runtimes: &mut self.services,
            current_pty_size: self.current_pty_size,
            events_tx: &self.events_tx,
            ui_tx: &self.ui_tx,
            shutdown: &self.shutdown,
        })
        .await
    }

    async fn handle_command(&mut self, services: &ServiceMap, command: Command) {
        match command {
            Command::Restart(service_id) => {
                if let Some(runtime) = self.services.get_mut(&service_id) {
                    // Logs are cleared when the service is actually (re)started in
                    // `start_service_if_ready`, after the old process has drained its output.
                    runtime.request_restart();
                }
            }
            Command::Enable(service_id) => {
                if let Some(runtime) = self.services.get_mut(&service_id) {
                    runtime.request_enable();
                }
            }
            Command::RestartAll => {
                for service_id in services.keys() {
                    if let Some(runtime) = self.services.get_mut(service_id)
                        && runtime.desired == DesiredState::Enabled
                    {
                        runtime.request_restart();
                    }
                }
            }
            Command::Disable(service_id) => {
                if let Some(runtime) = self.services.get_mut(&service_id) {
                    runtime.disable();
                    let _ = self.ui_tx.send(Event::Disabled(service_id)).await;
                }
            }
            Command::SendInput(service_id, data) => {
                if let Some(runtime) = self.services.get(&service_id)
                    && let Some(running) = &runtime.running
                {
                    running.pty.write_input(&service_id, &data);
                }
            }
            Command::ResizeAll { cols, rows } => {
                self.current_pty_size = portable_pty::PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                };
                for (service_id, runtime) in &self.services {
                    if let Some(running) = &runtime.running {
                        running.pty.resize(service_id, self.current_pty_size);
                    }
                }
            }
        }
    }

    async fn handle_event(
        &mut self,
        services: &ServiceMap,
        event: ProcessEvent,
    ) -> eyre::Result<()> {
        tracing::debug!(?event, "received process event");

        let service_id = event.service_id().clone();
        let Some(runtime) = self.services.get_mut(&service_id) else {
            return Ok(());
        };
        if runtime.current_run_id() != Some(event.run_id()) {
            tracing::debug!(
                service_id,
                event_run_id = ?event.run_id(),
                current_run_id = ?runtime.current_run_id(),
                "ignoring stale process event"
            );
            return Ok(());
        }

        let ui_event = event.into_ui_event();
        match &ui_event {
            Event::Healthy(_) => {
                if matches!(runtime.state, State::Running { .. } | State::Starting) {
                    runtime.state = State::Running {
                        health: Some(crate::health_check::Health::Healthy),
                    };
                }
            }
            Event::Unhealthy(_) => {
                if matches!(runtime.state, State::Running { .. } | State::Starting) {
                    runtime.state = State::Running {
                        health: Some(crate::health_check::Health::Unhealthy),
                    };
                }
            }
            Event::Killed(_) => {
                if runtime.desired == DesiredState::Enabled {
                    runtime.state = State::Killed;
                }
            }
            Event::Exited(_, exit_code) => {
                let Some(service) = services.get(&service_id) else {
                    return Ok(());
                };
                runtime.finish_current_run(&service.restart_policy, *exit_code);
            }
            Event::LogLine { .. }
            | Event::Started { .. }
            | Event::HealthCheckStarted { .. }
            | Event::HealthCheckLogLine { .. }
            | Event::HealthCheckFinished { .. }
            | Event::Disabled(_)
            | Event::ClearLogs(_) => {}
        }

        if matches!(&ui_event, Event::LogLine { .. }) {
            let _ = self.ui_tx.try_send(ui_event);
        } else {
            self.ui_tx.send(ui_event).await?;
        }
        Ok(())
    }

    fn next_backoff(&self) -> Option<tokio::time::Instant> {
        let now = tokio::time::Instant::now();
        self.services
            .values()
            .filter_map(|runtime| runtime.restart.backoff_until)
            .filter(|deadline| *deadline > now)
            .min()
    }

    fn running_count(&self) -> usize {
        self.services
            .values()
            .filter(|runtime| runtime.running.is_some())
            .count()
    }

    fn cancel_all_running(&self) {
        for runtime in self.services.values() {
            if let Some(running) = &runtime.running {
                running.cancel();
            }
        }
    }

    /// Keep the runtime alive after a shutdown so the per-service termination tasks can finish
    /// their SIGTERM -> deadline -> SIGKILL escalation and reap their children.
    ///
    /// Without this drain the tokio runtime would be dropped the instant the scheduler returns,
    /// aborting those detached tasks mid-escalation and orphaning any process that ignores
    /// SIGTERM. Each matching `Exited` event removes the service's run handle, so the drain ends as
    /// soon as every child has been reaped (bounded by an overall timeout).
    async fn drain_on_shutdown(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
    ) {
        if self.running_count() == 0 {
            return;
        }
        tracing::debug!(
            remaining = self.running_count(),
            "draining services on shutdown"
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while self.running_count() > 0 {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {
                    tracing::warn!(
                        remaining = self.running_count(),
                        "timed out waiting for services to exit"
                    );
                    break;
                }
                event = events_rx.recv() => {
                    let Some(event) = event else { break };
                    // Ignore UI-forward failures: the UI may already have shut down.
                    let _ = self.handle_event(services, event).await;
                }
            }
        }
    }
}

pub async fn scheduler(
    services: &ServiceMap,
    mut commands_rx: mpsc::Receiver<Command>,
    mut events_rx: mpsc::Receiver<ProcessEvent>,
    events_tx: mpsc::Sender<ProcessEvent>,
    ui_tx: mpsc::Sender<Event>,
    shutdown: CancellationToken,
) -> eyre::Result<()> {
    let graph = ServiceGraph::new(services)?;
    let mut rt = SchedulerRuntime::new(services, graph, events_tx, ui_tx, shutdown.clone());

    // Initial scheduling pass
    tracing::debug!("started initial scheduling pass");
    if let Err(err) = rt.schedule_pass(services).await {
        tracing::debug!(?err, "stopping scheduler after ui channel closed");
        rt.cancel_all_running();
        rt.drain_on_shutdown(services, &mut events_rx).await;
        return Ok(());
    }
    tracing::debug!("completed initial scheduling pass");

    // Whenever an event comes in, try to (re)start any services whose deps are now healthy
    loop {
        tracing::debug!("waiting for scheduling event");
        // Wake the loop when the nearest pending restart backoff expires; without this a
        // backed-off service would never restart unless some unrelated event happened to arrive.
        let next_backoff = rt.next_backoff();
        tokio::select! {
            () = shutdown.cancelled() => {
                tracing::debug!("exiting scheduler");
                break;
            }
            command = commands_rx.recv() => {
                let Some(command) = command else {
                    break;
                };
                rt.handle_command(services, command).await;
            }
            event = events_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                if let Err(err) = rt.handle_event(services, event).await {
                    tracing::debug!(?err, "stopping scheduler after ui channel closed");
                    break;
                }
            }
            () = async {
                match next_backoff {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }

        if let Err(err) = rt.schedule_pass(services).await {
            tracing::debug!(?err, "stopping scheduler after ui channel closed");
            break;
        }
    }

    rt.cancel_all_running();
    rt.drain_on_shutdown(services, &mut events_rx).await;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::config;
    use crate::service::Service;
    use color_eyre::eyre;
    use indexmap::IndexMap;
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::time::{Duration, timeout};
    use yaml_spanned::Spanned;

    fn spanned_string(value: &str) -> Spanned<String> {
        Spanned {
            span: yaml_spanned::spanned::Span::default(),
            inner: value.to_string(),
        }
    }

    fn service_config(name: &str, command: (&str, &[&str])) -> config::Service {
        config::Service {
            name: spanned_string(name),
            command: (
                spanned_string(command.0),
                command
                    .1
                    .iter()
                    .map(|v| spanned_string(v))
                    .collect::<Vec<_>>(),
            ),
            working_dir: None,
            env_file: vec![],
            environment: IndexMap::new(),
            depends_on: vec![],
            healthcheck: None,
            ports: vec![],
            restart: None,
            color: None,
        }
    }

    fn unique_tmp_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("micromux-{prefix}-{nanos}"))
    }

    async fn recv_event(
        mut rx: mpsc::Receiver<Event>,
    ) -> eyre::Result<(Event, mpsc::Receiver<Event>)> {
        let ev = timeout(Duration::from_secs(5), rx.recv())
            .await
            .map_err(|_| eyre::eyre!("timeout waiting for event"))?
            .ok_or_else(|| eyre::eyre!("event channel closed"))?;
        Ok((ev, rx))
    }

    fn healthcheck_always_ok() -> config::HealthCheck {
        config::HealthCheck {
            test: (
                spanned_string("sh"),
                vec![spanned_string("-c"), spanned_string("exit 0")],
            ),
            start_delay: None,
            interval: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: Duration::from_millis(25),
            }),
            timeout: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: Duration::from_millis(500),
            }),
            retries: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: 10,
            }),
        }
    }

    #[tokio::test]
    async fn healthcheck_inherits_environment() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut cfg = service_config("svc", ("sh", &["-c", "sleep 60"]));
        cfg.environment
            .insert(spanned_string("HC_FOO"), spanned_string("bar"));
        cfg.healthcheck = Some(config::HealthCheck {
            test: (
                spanned_string("sh"),
                vec![
                    spanned_string("-c"),
                    spanned_string("[ \"$HC_FOO\" = \"bar\" ]"),
                ],
            ),
            start_delay: None,
            interval: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: Duration::from_millis(25),
            }),
            timeout: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: Duration::from_millis(500),
            }),
            retries: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: 1,
            }),
        });

        let mut services: ServiceMap = ServiceMap::new();
        services.insert("svc".to_string(), Service::new("svc", config_dir, cfg)?);

        let shutdown = CancellationToken::new();
        let (ui_tx, mut ui_rx) = mpsc::channel(128);
        let (events_tx, events_rx) = mpsc::channel(128);
        let (_commands_tx, commands_rx) = mpsc::channel(128);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let mut saw_hc_success = false;
        for _ in 0..200 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if let Event::HealthCheckFinished { success: true, .. } = event {
                saw_hc_success = true;
                break;
            }
        }

        shutdown.cancel();
        handle.await??;
        assert!(saw_hc_success);
        Ok(())
    }

    #[tokio::test]
    async fn healthcheck_inherits_working_dir() -> eyre::Result<()> {
        let dir = unique_tmp_dir("healthcheck-cwd");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("marker.txt"), "ok")?;

        let mut cfg = service_config("svc", ("sh", &["-c", "sleep 60"]));
        cfg.working_dir = Some(spanned_string(dir.to_string_lossy().as_ref()));
        cfg.healthcheck = Some(config::HealthCheck {
            test: (
                spanned_string("sh"),
                vec![spanned_string("-c"), spanned_string("test -f marker.txt")],
            ),
            start_delay: None,
            interval: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: Duration::from_millis(25),
            }),
            timeout: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: Duration::from_millis(500),
            }),
            retries: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: 1,
            }),
        });

        let mut services: ServiceMap = ServiceMap::new();
        services.insert("svc".to_string(), Service::new("svc", &dir, cfg)?);

        let shutdown = CancellationToken::new();
        let (ui_tx, mut ui_rx) = mpsc::channel(128);
        let (events_tx, events_rx) = mpsc::channel(128);
        let (_commands_tx, commands_rx) = mpsc::channel(128);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let mut saw_hc_success = false;
        for _ in 0..200 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if let Event::HealthCheckFinished { success: true, .. } = event {
                saw_hc_success = true;
                break;
            }
        }

        shutdown.cancel();
        handle.await??;
        assert!(saw_hc_success);
        Ok(())
    }

    #[tokio::test]
    async fn healthcheck_spawn_error_emits_log_line() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut cfg = service_config("svc", ("sh", &["-c", "sleep 60"]));
        cfg.healthcheck = Some(config::HealthCheck {
            test: (
                spanned_string("definitely-not-a-real-binary"),
                vec![spanned_string("--version")],
            ),
            start_delay: None,
            interval: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: Duration::from_millis(25),
            }),
            timeout: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: Duration::from_millis(500),
            }),
            retries: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: 1,
            }),
        });

        let mut services: ServiceMap = ServiceMap::new();
        services.insert("svc".to_string(), Service::new("svc", config_dir, cfg)?);

        let shutdown = CancellationToken::new();
        let (ui_tx, mut ui_rx) = mpsc::channel(128);
        let (events_tx, events_rx) = mpsc::channel(128);
        let (_commands_tx, commands_rx) = mpsc::channel(128);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let mut saw_log_line = false;
        let mut saw_finished = false;
        for _ in 0..200 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            match event {
                Event::HealthCheckLogLine { stream, line, .. }
                    if matches!(stream, OutputStream::Stderr) && !line.is_empty() =>
                {
                    saw_log_line = true;
                }
                Event::HealthCheckFinished {
                    success: false,
                    exit_code: -1,
                    ..
                } => {
                    saw_finished = true;
                    if saw_log_line {
                        break;
                    }
                }
                _ => {}
            }
        }

        shutdown.cancel();
        handle.await??;
        assert!(saw_log_line);
        assert!(saw_finished);
        Ok(())
    }

    #[tokio::test]
    async fn disable_kills_running_service() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            "svc".to_string(),
            Service::new(
                "svc",
                config_dir,
                service_config("svc", ("sh", &["-c", "sleep 60"])),
            )?,
        );

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::channel(64);
        let (commands_tx, commands_rx) = mpsc::channel(64);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let (event, mut ui_rx) = recv_event(ui_rx).await?;
        assert!(matches!(event, Event::Started { .. }));

        commands_tx
            .send(Command::Disable("svc".to_string()))
            .await?;

        loop {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if matches!(event, Event::Killed(_) | Event::Exited(_, _)) {
                break;
            }
        }

        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn ui_drop_still_drains_running_service() -> eyre::Result<()> {
        use nix::errno::Errno;
        use nix::sys::signal::kill;
        use nix::unistd::Pid;

        let dir = unique_tmp_dir("ui-drop-drain");
        fs::create_dir_all(&dir)?;
        let pid_path = dir.join("pid");
        let command = format!(
            "trap '' TERM; echo $$ > {}; sleep 60",
            pid_path.to_string_lossy()
        );

        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            "svc".to_string(),
            Service::new(
                "svc",
                config_dir,
                service_config("svc", ("sh", &["-c", &command])),
            )?,
        );

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::channel(64);
        let (commands_tx, commands_rx) = mpsc::channel(64);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let (event, ui_rx) = recv_event(ui_rx).await?;
        assert!(matches!(event, Event::Started { .. }));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let pid = loop {
            if let Ok(pid_str) = fs::read_to_string(&pid_path) {
                break pid_str.trim().parse::<i32>()?;
            }
            if tokio::time::Instant::now() >= deadline {
                eyre::bail!("service did not write pid file");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        drop(ui_rx);
        commands_tx
            .send(Command::Disable("svc".to_string()))
            .await?;

        timeout(Duration::from_secs(3), handle)
            .await
            .map_err(|_| eyre::eyre!("scheduler did not drain after ui drop"))???;

        match kill(Pid::from_raw(pid), None) {
            Err(Errno::ESRCH) => {}
            other => eyre::bail!("expected service process to be reaped, got {other:?}"),
        }

        Ok(())
    }

    #[tokio::test]
    async fn restart_restarts_service() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            "svc".to_string(),
            Service::new(
                "svc",
                config_dir,
                service_config("svc", ("sh", &["-c", "sleep 60"])),
            )?,
        );

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::channel(64);
        let (commands_tx, commands_rx) = mpsc::channel(64);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let (event, mut ui_rx) = recv_event(ui_rx).await?;
        assert!(matches!(event, Event::Started { .. }));

        commands_tx
            .send(Command::Restart("svc".to_string()))
            .await?;

        let mut saw_second_start = false;
        for _ in 0..10 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if matches!(event, Event::Started { .. }) {
                saw_second_start = true;
                break;
            }
        }

        assert!(saw_second_start);
        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn auto_restarts_failing_service_without_manual_command() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut cfg = service_config("svc", ("sh", &["-c", "exit 1"]));
        cfg.restart = Some(crate::service::RestartPolicy::Always);

        let mut services: ServiceMap = ServiceMap::new();
        services.insert("svc".to_string(), Service::new("svc", config_dir, cfg)?);

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(256);
        let (events_tx, events_rx) = mpsc::channel(256);
        let (_commands_tx, commands_rx) = mpsc::channel(256);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        // The service exits non-zero immediately and has no healthcheck or chatty neighbors,
        // so only the backoff timer can wake the scheduler to restart it. Seeing a second
        // Started with no manual Restart command proves the timer fires.
        let mut starts = 0;
        let mut ui_rx = ui_rx;
        for _ in 0..100 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if matches!(event, Event::Started { .. }) {
                starts += 1;
                if starts >= 2 {
                    break;
                }
            }
        }

        assert!(starts >= 2, "expected an automatic restart after a crash");
        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn stale_log_from_previous_run_is_ignored() -> eyre::Result<()> {
        let dir = unique_tmp_dir("stale-run-log");
        fs::create_dir_all(&dir)?;
        let marker = dir.join("run-count");
        let script = format!(
            "n=$(cat {marker} 2>/dev/null || echo 0); \
             n=$((n + 1)); \
             echo \"$n\" > {marker}; \
             if [ \"$n\" = 1 ]; then \
               (trap '' HUP TERM; sleep 0.7; echo stale-from-first-run) & \
               exit 1; \
             else \
               echo second-run; \
               sleep 60; \
             fi",
            marker = marker.to_string_lossy()
        );

        let mut cfg = service_config("svc", ("sh", &["-c", "true"]));
        cfg.command = (
            spanned_string("sh"),
            vec![spanned_string("-c"), spanned_string(&script)],
        );
        cfg.restart = Some(crate::service::RestartPolicy::Always);

        let mut services: ServiceMap = ServiceMap::new();
        services.insert("svc".to_string(), Service::new("svc", &dir, cfg)?);

        let shutdown = CancellationToken::new();
        let (ui_tx, mut ui_rx) = mpsc::channel(256);
        let (events_tx, events_rx) = mpsc::channel(256);
        let (_commands_tx, commands_rx) = mpsc::channel(256);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let mut starts = 0;
        let mut saw_second_run = false;
        for _ in 0..100 {
            let event = timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .map_err(|_| eyre::eyre!("timeout waiting for event"))?
                .ok_or_else(|| eyre::eyre!("event channel closed"))?;
            match event {
                Event::Started { .. } => {
                    starts += 1;
                }
                Event::LogLine { line, .. } if line.contains("second-run") => {
                    saw_second_run = true;
                }
                Event::LogLine { line, .. } if line.contains("stale-from-first-run") => {
                    eyre::bail!("stale first-run output reached the UI");
                }
                _ => {}
            }

            if starts >= 2 && saw_second_run {
                break;
            }
        }
        assert!(starts >= 2);
        assert!(saw_second_run);

        let deadline = tokio::time::Instant::now() + Duration::from_millis(1200);
        while tokio::time::Instant::now() < deadline {
            match timeout(Duration::from_millis(100), ui_rx.recv()).await {
                Ok(Some(Event::LogLine { line, .. })) if line.contains("stale-from-first-run") => {
                    eyre::bail!("stale first-run output reached the UI");
                }
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => break,
            }
        }

        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn pty_reader_exits_after_child_exit_when_grandchild_holds_slave() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let service_id = "leaky-pty-reader".to_string();
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            service_id.clone(),
            Service::new(
                &service_id,
                config_dir,
                service_config(
                    &service_id,
                    (
                        "sh",
                        &["-c", "(trap '' HUP TERM; sleep 2) & sleep 0.1; exit 0"],
                    ),
                ),
            )?,
        );

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(128);
        let (events_tx, events_rx) = mpsc::channel(128);
        let (_commands_tx, commands_rx) = mpsc::channel(128);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let run_id = RunId::new(1);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        while tokio::time::Instant::now() < deadline && !pty::log_reader_active(&service_id, run_id)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(pty::log_reader_active(&service_id, run_id));

        let mut ui_rx = ui_rx;
        let mut saw_exit = false;
        for _ in 0..20 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if matches!(event, Event::Exited(id, 0) if id == service_id) {
                saw_exit = true;
                break;
            }
        }
        assert!(saw_exit);

        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        while tokio::time::Instant::now() < deadline && pty::log_reader_active(&service_id, run_id)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !pty::log_reader_active(&service_id, run_id),
            "pty reader stayed alive after run ownership ended"
        );

        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn enable_after_disable_starts_service_again() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            "svc".to_string(),
            Service::new(
                "svc",
                config_dir,
                service_config("svc", ("sh", &["-c", "sleep 60"])),
            )?,
        );

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(128);
        let (events_tx, events_rx) = mpsc::channel(128);
        let (commands_tx, commands_rx) = mpsc::channel(128);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let (event, mut ui_rx) = recv_event(ui_rx).await?;
        assert!(matches!(event, Event::Started { .. }));

        commands_tx
            .send(Command::Disable("svc".to_string()))
            .await?;

        loop {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if matches!(event, Event::Exited(_, _)) {
                break;
            }
        }

        commands_tx.send(Command::Enable("svc".to_string())).await?;

        let mut restarted = false;
        for _ in 0..20 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if matches!(event, Event::Started { .. }) {
                restarted = true;
                break;
            }
        }

        assert!(restarted);
        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn enable_after_disable_does_not_clear_logs() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            "svc".to_string(),
            Service::new(
                "svc",
                config_dir,
                service_config("svc", ("sh", &["-c", "echo first-run; sleep 60"])),
            )?,
        );

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(128);
        let (events_tx, events_rx) = mpsc::channel(128);
        let (commands_tx, commands_rx) = mpsc::channel(128);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let mut ui_rx = ui_rx;
        let mut saw_log = false;
        for _ in 0..20 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if matches!(event, Event::LogLine { line, .. } if line.contains("first-run")) {
                saw_log = true;
                break;
            }
        }
        assert!(saw_log);

        commands_tx
            .send(Command::Disable("svc".to_string()))
            .await?;
        loop {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if matches!(event, Event::Exited(_, _)) {
                break;
            }
        }

        commands_tx.send(Command::Enable("svc".to_string())).await?;

        let mut restarted = false;
        for _ in 0..20 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            match event {
                Event::ClearLogs(_) => eyre::bail!("enable unexpectedly cleared logs"),
                Event::Started { .. } => {
                    restarted = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(restarted);
        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn restart_all_skips_disabled_services() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            "enabled".to_string(),
            Service::new(
                "enabled",
                config_dir,
                service_config("enabled", ("sh", &["-c", "sleep 60"])),
            )?,
        );
        services.insert(
            "disabled".to_string(),
            Service::new(
                "disabled",
                config_dir,
                service_config("disabled", ("sh", &["-c", "sleep 60"])),
            )?,
        );

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(256);
        let (events_tx, events_rx) = mpsc::channel(256);
        let (commands_tx, commands_rx) = mpsc::channel(256);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let mut ui_rx = ui_rx;
        let mut started = std::collections::HashSet::new();
        while started.len() < 2 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if let Event::Started { service_id } = event {
                started.insert(service_id);
            }
        }

        commands_tx
            .send(Command::Disable("disabled".to_string()))
            .await?;
        loop {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            if matches!(event, Event::Exited(service_id, _) if service_id == "disabled") {
                break;
            }
        }

        commands_tx.send(Command::RestartAll).await?;

        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        let mut saw_enabled_restart = false;
        while tokio::time::Instant::now() < deadline {
            match timeout(Duration::from_millis(100), ui_rx.recv()).await {
                Ok(Some(Event::Started { service_id })) if service_id == "disabled" => {
                    eyre::bail!("RestartAll restarted a disabled service");
                }
                Ok(Some(Event::Started { service_id })) if service_id == "enabled" => {
                    saw_enabled_restart = true;
                }
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => break,
            }
        }

        assert!(saw_enabled_restart);
        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn disabling_dependency_blocks_pending_dependents_immediately() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();

        services.insert(
            "dep".to_string(),
            Service::new(
                "dep",
                config_dir,
                service_config("dep", ("sh", &["-c", "trap '' TERM; sleep 5"])),
            )?,
        );

        services.insert(
            "gate".to_string(),
            Service::new(
                "gate",
                config_dir,
                service_config("gate", ("sh", &["-c", "sleep 0.4; exit 0"])),
            )?,
        );

        let mut app_cfg = service_config("app", ("sh", &["-c", "echo app-started; sleep 60"]));
        app_cfg.depends_on = vec![
            config::Dependency {
                name: spanned_string("dep"),
                condition: Some(Spanned {
                    span: yaml_spanned::spanned::Span::default(),
                    inner: config::DependencyCondition::Started,
                }),
            },
            config::Dependency {
                name: spanned_string("gate"),
                condition: Some(Spanned {
                    span: yaml_spanned::spanned::Span::default(),
                    inner: config::DependencyCondition::CompletedSuccessfully,
                }),
            },
        ];
        services.insert("app".to_string(), Service::new("app", config_dir, app_cfg)?);

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(256);
        let (events_tx, events_rx) = mpsc::channel(256);
        let (commands_tx, commands_rx) = mpsc::channel(256);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let (event, mut ui_rx) = recv_event(ui_rx).await?;
        assert!(matches!(event, Event::Started { service_id } if service_id == "dep"));

        commands_tx
            .send(Command::Disable("dep".to_string()))
            .await?;

        let deadline = tokio::time::Instant::now() + Duration::from_millis(700);
        while tokio::time::Instant::now() < deadline {
            match timeout(Duration::from_millis(100), ui_rx.recv()).await {
                Ok(Some(Event::Started { service_id })) if service_id == "app" => {
                    eyre::bail!("dependent app started after its dependency was disabled");
                }
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => break,
            }
        }

        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn emits_log_lines() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            "svc".to_string(),
            Service::new(
                "svc",
                config_dir,
                service_config("svc", ("sh", &["-c", "echo hello && echo err >&2"])),
            )?,
        );

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::channel(64);
        let (_commands_tx, commands_rx) = mpsc::channel(64);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let (_event, mut ui_rx) = recv_event(ui_rx).await?;

        let mut saw_hello = false;
        let mut saw_err = false;

        for _ in 0..50 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            match event {
                Event::LogLine { line, .. } if line.contains("hello") => {
                    saw_hello = true;
                }
                Event::LogLine { line, .. } if line.contains("err") => {
                    saw_err = true;
                }
                Event::Exited(_, _) if saw_hello && saw_err => break,
                _ => {}
            }
        }

        assert!(saw_hello);
        assert!(saw_err);

        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn child_sees_tty() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            "svc".to_string(),
            Service::new(
                "svc",
                config_dir,
                service_config(
                    "svc",
                    (
                        "sh",
                        &["-c", "if tty -s; then echo tty; else echo notty; fi"],
                    ),
                ),
            )?,
        );

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::channel(64);
        let (_commands_tx, commands_rx) = mpsc::channel(64);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let (_event, mut ui_rx) = recv_event(ui_rx).await?;

        let mut saw_tty = false;
        for _ in 0..20 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            match event {
                Event::LogLine { line, .. } if line.contains("tty") => {
                    saw_tty = true;
                }
                Event::Exited(_, _) if saw_tty => break,
                _ => {}
            }
        }

        assert!(saw_tty);
        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn send_input_reaches_process() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            "svc".to_string(),
            Service::new(
                "svc",
                config_dir,
                service_config("svc", ("sh", &["-c", "read line; echo got:$line"])),
            )?,
        );

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::channel(64);
        let (commands_tx, commands_rx) = mpsc::channel(64);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let (_event, mut ui_rx) = recv_event(ui_rx).await?;

        commands_tx
            .send(Command::SendInput("svc".to_string(), b"hello\r".to_vec()))
            .await?;

        let mut saw = false;
        for _ in 0..30 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            match event {
                Event::LogLine { line, .. } if line.contains("got:hello") => {
                    saw = true;
                }
                Event::Exited(_, _) => {
                    if saw {
                        break;
                    }
                    return Err(eyre::eyre!("process exited before receiving input"));
                }
                _ => {}
            }
        }

        assert!(saw);
        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn depends_on_service_healthy_delays_start() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();

        let mut dep_cfg = service_config("dep", ("sh", &["-c", "sleep 60"]));
        dep_cfg.healthcheck = Some(healthcheck_always_ok());
        services.insert("dep".to_string(), Service::new("dep", config_dir, dep_cfg)?);

        let mut app_cfg = service_config("app", ("sh", &["-c", "sleep 60"]));
        app_cfg.depends_on = vec![config::Dependency {
            name: spanned_string("dep"),
            condition: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: config::DependencyCondition::Healthy,
            }),
        }];
        services.insert("app".to_string(), Service::new("app", config_dir, app_cfg)?);

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(256);
        let (events_tx, events_rx) = mpsc::channel(256);
        let (_commands_tx, commands_rx) = mpsc::channel(256);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let (event, mut ui_rx) = recv_event(ui_rx).await?;
        assert!(matches!(event, Event::Started { service_id } if service_id == "dep"));

        let mut saw_app_started = false;
        let mut saw_dep_healthy = false;
        for _ in 0..40 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            match event {
                Event::Healthy(service_id) if service_id == "dep" => {
                    saw_dep_healthy = true;
                }
                Event::Started { service_id } if service_id == "app" => {
                    saw_app_started = true;
                    break;
                }
                _ => {}
            }
        }

        assert!(saw_dep_healthy);
        assert!(saw_app_started);

        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn resize_all_changes_stty_size_for_new_service() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let mut services: ServiceMap = ServiceMap::new();

        services.insert(
            "dep".to_string(),
            Service::new(
                "dep",
                config_dir,
                service_config("dep", ("sh", &["-c", "exit 0"])),
            )?,
        );

        let mut app_cfg = service_config("app", ("sh", &["-c", "stty size; read line; stty size"]));
        app_cfg.depends_on = vec![config::Dependency {
            name: spanned_string("dep"),
            condition: Some(Spanned {
                span: yaml_spanned::spanned::Span::default(),
                inner: config::DependencyCondition::CompletedSuccessfully,
            }),
        }];
        services.insert("app".to_string(), Service::new("app", config_dir, app_cfg)?);

        let shutdown = CancellationToken::new();
        let (ui_tx, ui_rx) = mpsc::channel(256);
        let (events_tx, events_rx) = mpsc::channel(256);
        let (commands_tx, commands_rx) = mpsc::channel(256);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        commands_tx
            .send(Command::ResizeAll {
                cols: 100,
                rows: 40,
            })
            .await?;

        let (_event, mut ui_rx) = recv_event(ui_rx).await?;

        let mut saw_first = false;
        let mut saw_second = false;
        for _ in 0..80 {
            let (event, next_rx) = recv_event(ui_rx).await?;
            ui_rx = next_rx;
            match event {
                Event::Started { service_id } if service_id == "app" => {}
                Event::LogLine {
                    service_id, line, ..
                } if service_id == "app" && line.trim() == "40 100" => {
                    if saw_first {
                        saw_second = true;
                        break;
                    }

                    saw_first = true;
                    commands_tx
                        .send(Command::SendInput("app".to_string(), b"go\r".to_vec()))
                        .await?;
                }
                _ => {}
            }
        }

        assert!(saw_first);
        assert!(saw_second);
        shutdown.cancel();
        handle.await??;
        Ok(())
    }

    #[tokio::test]
    async fn working_dir_is_used_for_spawn() -> eyre::Result<()> {
        let base = unique_tmp_dir("working-dir");
        fs::create_dir_all(&base)?;
        let config_dir = base.join("cfg");
        let work_rel = "work";
        let work_abs = config_dir.join(work_rel);
        fs::create_dir_all(&work_abs)?;

        let mut cfg = service_config("svc", ("sh", &["-c", "pwd"]));
        cfg.working_dir = Some(spanned_string(work_rel));

        let mut services: ServiceMap = ServiceMap::new();
        services.insert("svc".to_string(), Service::new("svc", &config_dir, cfg)?);

        let shutdown = CancellationToken::new();
        let (ui_tx, mut ui_rx) = mpsc::channel(64);
        let (events_tx, events_rx) = mpsc::channel(64);
        let (_commands_tx, commands_rx) = mpsc::channel(64);

        let handle = tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                scheduler(
                    &services,
                    commands_rx,
                    events_rx,
                    events_tx,
                    ui_tx,
                    shutdown,
                )
                .await
            }
        });

        let mut saw_pwd = false;
        let expected = work_abs.canonicalize()?;

        for _ in 0..50 {
            let ev = timeout(Duration::from_secs(5), ui_rx.recv())
                .await
                .map_err(|_| eyre::eyre!("timeout waiting for event"))?
                .ok_or_else(|| eyre::eyre!("event channel closed"))?;
            match ev {
                Event::LogLine { line, .. } => {
                    if Path::new(&line) == expected {
                        saw_pwd = true;
                        break;
                    }
                }
                Event::Exited(_, _) if saw_pwd => break,
                _ => {}
            }
        }

        assert!(
            saw_pwd,
            "did not observe expected pwd output {}",
            expected.display()
        );
        shutdown.cancel();
        handle.await??;
        Ok(())
    }
}
