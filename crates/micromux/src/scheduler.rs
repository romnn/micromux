use crate::{
    ServiceMap,
    graph::ServiceGraph,
    service::{self},
};
use color_eyre::eyre;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[path = "scheduler/types.rs"]
mod types;
pub use types::{Command, Event, LogUpdateKind, OutputStream, ServiceID, State};

#[path = "scheduler/pty.rs"]
mod pty;

#[path = "scheduler/schedule.rs"]
mod schedule;

struct SchedulerRuntime<'a> {
    graph: ServiceGraph<'a>,
    desired_disabled: HashSet<ServiceID>,
    restart_requested: HashSet<ServiceID>,
    terminate_tokens: HashMap<ServiceID, CancellationToken>,
    pty_masters: HashMap<ServiceID, Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>>,
    pty_writers: HashMap<ServiceID, Arc<Mutex<Box<dyn Write + Send>>>>,
    pty_sizes: HashMap<ServiceID, Arc<std::sync::atomic::AtomicU32>>,
    current_pty_size: portable_pty::PtySize,
    restart_backoff_until: HashMap<ServiceID, tokio::time::Instant>,
    restart_backoff_delay: HashMap<ServiceID, Duration>,
    restart_on_failure_remaining: HashMap<ServiceID, usize>,
    service_state: HashMap<ServiceID, State>,
    events_tx: mpsc::Sender<Event>,
    ui_tx: mpsc::Sender<Event>,
    shutdown: CancellationToken,
}

impl<'a> SchedulerRuntime<'a> {
    fn new(
        services: &ServiceMap,
        graph: ServiceGraph<'a>,
        events_tx: mpsc::Sender<Event>,
        ui_tx: mpsc::Sender<Event>,
        shutdown: CancellationToken,
    ) -> Self {
        let restart_on_failure_remaining: HashMap<ServiceID, usize> = services
            .iter()
            .filter_map(|(service_id, service)| match service.restart_policy {
                service::RestartPolicy::OnFailure { remaining_attempts } => {
                    Some((service_id.clone(), remaining_attempts))
                }
                _ => None,
            })
            .collect();

        let service_state: HashMap<ServiceID, State> = services
            .keys()
            .map(|service_id| (service_id.clone(), State::Pending))
            .collect();

        Self {
            graph,
            desired_disabled: HashSet::new(),
            restart_requested: HashSet::new(),
            terminate_tokens: HashMap::new(),
            pty_masters: HashMap::new(),
            pty_writers: HashMap::new(),
            pty_sizes: HashMap::new(),
            current_pty_size: portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            restart_backoff_until: HashMap::new(),
            restart_backoff_delay: HashMap::new(),
            restart_on_failure_remaining,
            service_state,
            events_tx,
            ui_tx,
            shutdown,
        }
    }

    fn schedule_pass(&mut self, services: &ServiceMap) {
        schedule::schedule_ready(&mut schedule::ScheduleContext {
            services,
            graph: &self.graph.inner,
            service_state: &mut self.service_state,
            desired_disabled: &self.desired_disabled,
            restart_requested: &mut self.restart_requested,
            restart_on_failure_remaining: &mut self.restart_on_failure_remaining,
            terminate_tokens: &mut self.terminate_tokens,
            pty_masters: &mut self.pty_masters,
            pty_writers: &mut self.pty_writers,
            pty_sizes: &mut self.pty_sizes,
            current_pty_size: self.current_pty_size,
            restart_backoff_until: &self.restart_backoff_until,
            events_tx: &self.events_tx,
            ui_tx: &self.ui_tx,
            shutdown: &self.shutdown,
        });
    }

    fn apply_restart_backoff(&mut self, service_id: &ServiceID, code: i32) {
        if code != 0
            && !self.desired_disabled.contains(service_id)
            && !self.restart_requested.contains(service_id)
        {
            let delay = self
                .restart_backoff_delay
                .entry(service_id.clone())
                .and_modify(|d| {
                    *d = (*d * 2).min(Duration::from_secs(10));
                })
                .or_insert(Duration::from_millis(250));
            self.restart_backoff_until
                .insert(service_id.clone(), tokio::time::Instant::now() + *delay);
        } else {
            self.restart_backoff_until.remove(service_id);
            self.restart_backoff_delay.remove(service_id);
        }
    }

    async fn handle_command(&mut self, services: &ServiceMap, command: Command) {
        match command {
            Command::Restart(service_id) | Command::Enable(service_id) => {
                self.desired_disabled.remove(&service_id);
                self.restart_requested.insert(service_id.clone());
                self.restart_backoff_until.remove(&service_id);
                self.restart_backoff_delay.remove(&service_id);
                let _ = self.ui_tx.try_send(Event::ClearLogs(service_id.clone()));
                if let Some(terminate) = self.terminate_tokens.get(&service_id) {
                    terminate.cancel();
                }
            }
            Command::RestartAll => {
                for service_id in services.keys() {
                    self.desired_disabled.remove(service_id);
                    self.restart_requested.insert(service_id.clone());
                    self.restart_backoff_until.remove(service_id);
                    self.restart_backoff_delay.remove(service_id);
                    let _ = self.ui_tx.try_send(Event::ClearLogs(service_id.clone()));
                    if let Some(terminate) = self.terminate_tokens.get(service_id) {
                        terminate.cancel();
                    }
                }
            }
            Command::Disable(service_id) => {
                self.desired_disabled.insert(service_id.clone());
                let _ = self.ui_tx.send(Event::Disabled(service_id.clone())).await;
                self.restart_backoff_until.remove(&service_id);
                self.restart_backoff_delay.remove(&service_id);
                if let Some(terminate) = self.terminate_tokens.get(&service_id) {
                    terminate.cancel();
                }
            }
            Command::SendInput(service_id, data) => {
                let Some(writer) = self.pty_writers.get(&service_id) else {
                    return;
                };
                let mut guard = writer.lock();
                if let Err(err) = guard.write_all(&data) {
                    tracing::warn!(?err, service_id, "failed to write to pty");
                }
                if let Err(err) = guard.flush() {
                    tracing::warn!(?err, service_id, "failed to flush pty");
                }
            }
            Command::ResizeAll { cols, rows } => {
                self.current_pty_size = portable_pty::PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                };
                for (service_id, master) in &self.pty_masters {
                    let guard = master.lock();
                    let res = guard.resize(portable_pty::PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                    if let Err(err) = res {
                        tracing::warn!(?err, service_id, "failed to resize pty");
                    }
                }

                let packed = (u32::from(rows) << 16) | u32::from(cols);
                for size in self.pty_sizes.values() {
                    size.store(packed, Ordering::Relaxed);
                }
            }
        }
    }

    async fn handle_event(&mut self, services: &ServiceMap, event: Event) -> eyre::Result<()> {
        tracing::debug!(%event, "received event");

        let service_id = event.service_id().clone();
        schedule::update_state(services, &mut self.service_state, &event);

        if let Event::Exited(_, code) = &event {
            self.terminate_tokens.remove(&service_id);
            self.pty_masters.remove(&service_id);
            self.pty_writers.remove(&service_id);
            self.pty_sizes.remove(&service_id);
            self.apply_restart_backoff(&service_id, *code);
        }

        if matches!(&event, Event::LogLine { .. }) {
            let _ = self.ui_tx.try_send(event);
        } else {
            self.ui_tx.send(event).await?;
        }
        Ok(())
    }
}

pub async fn scheduler(
    services: &ServiceMap,
    mut commands_rx: mpsc::Receiver<Command>,
    mut events_rx: mpsc::Receiver<Event>,
    events_tx: mpsc::Sender<Event>,
    ui_tx: mpsc::Sender<Event>,
    shutdown: CancellationToken,
) -> eyre::Result<()> {
    let graph = ServiceGraph::new(services)?;
    let mut rt = SchedulerRuntime::new(services, graph, events_tx, ui_tx, shutdown.clone());

    // Initial scheduling pass
    tracing::debug!("started initial scheduling pass");
    rt.schedule_pass(services);
    tracing::debug!("completed initial scheduling pass");

    // Whenever an event comes in, try to (re)start any services whose deps are now healthy
    loop {
        tracing::debug!("waiting for scheduling event");
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
                rt.handle_event(services, event).await?;
            }
        }

        rt.schedule_pass(services);
    }
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
                Event::Exited(_, _) => {
                    if saw_tty {
                        break;
                    }
                }
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
                } if service_id == "app" => {
                    if line.trim() == "40 100" {
                        if saw_first {
                            saw_second = true;
                            break;
                        }

                        saw_first = true;
                        commands_tx
                            .send(Command::SendInput("app".to_string(), b"go\r".to_vec()))
                            .await?;
                    }
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
