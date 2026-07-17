use super::*;
use crate::config;
use crate::service::Service;
use color_eyre::eyre;
use indexmap::IndexMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{Duration, timeout};
use yaml_spanned::Spanned;

use crate::model::{Desired, Execution};
use similar_asserts::assert_eq;

/// Run the real scheduler with a test-only transition event sink.
async fn run_test_scheduler(
    services: &ServiceMap,
    commands_rx: mpsc::Receiver<Command>,
    events_rx: mpsc::Receiver<ProcessEvent>,
    events_tx: mpsc::Sender<ProcessEvent>,
    test_events_tx: mpsc::Sender<Event>,
    shutdown: CancellationToken,
) -> eyre::Result<()> {
    let (_reader, writer) = crate::model::new(crate::initial_model_entries(services));
    scheduler(SchedulerInput {
        services: services.clone(),
        reload_config: None,
        commands_rx,
        events_rx,
        events_tx,
        test_events_tx: Some(test_events_tx),
        writer,
        shutdown,
    })
    .await
}

/// A scheduler running against a temp config plus model/control handles.
struct Harness {
    reader: crate::model::SessionModelReader,
    control: ServiceControl,
    shutdown: CancellationToken,
    handle: tokio::task::JoinHandle<eyre::Result<()>>,
}

fn spawn_harness(services: ServiceMap) -> Harness {
    let (commands_tx, commands_rx) = mpsc::channel(64);
    let (events_tx, events_rx) = mpsc::channel(256);
    let (reader, writer) = crate::model::new(crate::initial_model_entries(&services));
    let control = ServiceControl::new(commands_tx);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            scheduler(SchedulerInput {
                services,
                reload_config: None,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx: None,
                writer,
                shutdown,
            })
            .await
        }
    });
    Harness {
        reader,
        control,
        shutdown,
        handle,
    }
}

fn spawn_harness_with_reload(services: ServiceMap, config_path: PathBuf) -> Harness {
    let (commands_tx, commands_rx) = mpsc::channel(64);
    let (events_tx, events_rx) = mpsc::channel(256);
    let (reader, writer) = crate::model::new(crate::initial_model_entries(&services));
    let control = ServiceControl::new(commands_tx);
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            scheduler(SchedulerInput {
                services,
                reload_config: Some(ReloadConfig {
                    config_path,
                    strict_override: None,
                }),
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx: None,
                writer,
                shutdown,
            })
            .await
        }
    });
    Harness {
        reader,
        control,
        shutdown,
        handle,
    }
}

fn accepted(
    res: Result<ServiceCommandResult, SchedulerStopped>,
) -> eyre::Result<Vec<ServiceCommandAck>> {
    res.map_err(|_| eyre::eyre!("scheduler stopped"))?
        .map_err(|rejection| eyre::eyre!("unexpected rejection: {rejection}"))
}

async fn wait_until<F>(
    reader: &crate::model::SessionModelReader,
    id: &str,
    mut predicate: F,
) -> eyre::Result<crate::model::ServiceSnapshot>
where
    F: FnMut(&crate::model::ServiceSnapshot) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(snapshot) = reader.service(id)
            && predicate(&snapshot)
        {
            return Ok(snapshot);
        }
        if tokio::time::Instant::now() >= deadline {
            eyre::bail!("timed out waiting for a condition on `{id}`");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_log(
    reader: &crate::model::SessionModelReader,
    id: &str,
    needle: &str,
) -> eyre::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if reader
            .logs(id, None)
            .iter()
            .any(|line| line.line.contains(needle))
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            eyre::bail!("timed out waiting for `{needle}` in `{id}` logs");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn project_execution_maps_the_desired_execution_table() {
    assert_eq!(
        project_execution(false, &State::Pending, false),
        Execution::Pending
    );
    assert_eq!(
        project_execution(true, &State::Starting, false),
        Execution::Starting
    );
    assert_eq!(
        project_execution(true, &State::Running { health: None }, false),
        Execution::Running
    );
    assert_eq!(
        project_execution(true, &State::Killed, false),
        Execution::Stopping
    );
    // The decisive row: a disabled service still draining is Stopping, not already-Exited.
    assert_eq!(
        project_execution(true, &State::Disabled, true),
        Execution::Stopping
    );
    assert_eq!(
        project_execution(false, &State::Exited { exit_code: 0 }, true),
        Execution::Exited
    );
    assert_eq!(
        project_execution(false, &State::Disabled, true),
        Execution::Exited
    );
    assert_eq!(
        project_execution(false, &State::Disabled, false),
        Execution::Pending
    );
}

#[test]
fn failed_start_advances_generation_and_records_exit() {
    let mut runtime = ServiceRuntime::new(ServiceRuntimeInit {
        restart_policy: &crate::service::RestartPolicy::Never,
        startup_mode: StartupMode::Enabled,
    });
    runtime.mark_starting();
    let run_id = runtime.allocate_run_id();

    runtime.finish_failed_start(&crate::service::RestartPolicy::Never, run_id, -1);

    assert_eq!(runtime.run_generation(), 1);
    assert_eq!(runtime.last_exit_code, Some(-1));
    assert!(matches!(runtime.state, State::Exited { exit_code: -1 }));
}

/// Start a fake run on `runtime` without spawning a process, for handle-bookkeeping tests.
fn start_dummy_run(runtime: &mut ServiceRuntime) -> eyre::Result<RunId> {
    runtime.mark_starting();
    let run_id = runtime.allocate_run_id();
    runtime.mark_started(RunningService {
        run_id,
        terminate: CancellationToken::new(),
        log_reader: Some(pty::LogReaderHandle::test_dummy()),
        pty: pty::PtyHandles::test_dummy()?,
        since: tokio::time::Instant::now(),
    });
    Ok(run_id)
}

/// `LogReaderFinished` and `Exited` race through the same events channel from two producers, so
/// either order must leave no parked reader handle behind — a leaked handle holds a pipe fd, and
/// with the reader-finishes-first order (the common one: PTY EOF races the child reap, and every
/// manual restart/disable cancels the reader before the exit) it would accumulate one fd per run.
#[tokio::test]
async fn log_reader_finished_leaves_no_draining_handle_in_either_order() -> eyre::Result<()> {
    let policy = crate::service::RestartPolicy::Never;
    let mut runtime = ServiceRuntime::new(ServiceRuntimeInit {
        restart_policy: &policy,
        startup_mode: StartupMode::Enabled,
    });

    // Reader finishes first (EOF/cancel before the exit is processed).
    let run_id = start_dummy_run(&mut runtime)?;
    runtime.finish_log_reader(run_id);
    runtime.finish_current_run(&policy, 0);
    assert!(runtime.draining_log_readers.is_empty());

    // Exit is processed first; the reader keeps draining until its finish event removes it.
    let run_id = start_dummy_run(&mut runtime)?;
    runtime.finish_current_run(&policy, 0);
    assert_eq!(runtime.draining_log_readers.len(), 1);
    runtime.finish_log_reader(run_id);
    assert!(runtime.draining_log_readers.is_empty());

    Ok(())
}

#[tokio::test]
async fn initially_disabled_service_stays_stopped_until_enabled() -> eyre::Result<()> {
    let mut config = service_config("svc", ("sh", &["-c", "sleep 60"]));
    config.startup_mode = StartupMode::Disabled;
    let mut services: ServiceMap = ServiceMap::new();
    services.insert(
        "svc".to_string(),
        Service::new("svc", Path::new("."), config)?,
    );
    let harness = spawn_harness(services);
    let id = "svc".to_string();

    let snapshot = harness
        .reader
        .service("svc")
        .ok_or_else(|| eyre::eyre!("missing service snapshot"))?;
    assert_eq!(snapshot.desired, Desired::Disabled);
    assert_eq!(snapshot.execution, Execution::Pending);
    assert_eq!(snapshot.run_generation, 0);

    let rejected = harness
        .control
        .restart(&id)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(matches!(rejected, Err(CommandRejection::InvalidState)));

    accepted(harness.control.enable(&id).await)?;
    let snapshot = wait_until(&harness.reader, "svc", |snapshot| {
        snapshot.desired == Desired::Enabled && snapshot.execution == Execution::Running
    })
    .await?;
    assert_eq!(snapshot.run_generation, 1);

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn service_control_latches_generation_and_rejects_restart_when_disabled() -> eyre::Result<()>
{
    let mut services: ServiceMap = ServiceMap::new();
    services.insert(
        "svc".to_string(),
        Service::new(
            "svc",
            Path::new("."),
            service_config("svc", ("sh", &["-c", "sleep 60"])),
        )?,
    );
    let harness = spawn_harness(services);
    let id = "svc".to_string();

    let snapshot = wait_until(&harness.reader, "svc", |s| {
        s.execution == Execution::Running
    })
    .await?;
    assert_eq!(snapshot.run_generation, 1);

    // Restart acks the *pre-restart* generation; a new run bumps it to 2.
    let acks = accepted(harness.control.restart(&id).await)?;
    assert_eq!(acks.first().map(|a| a.observed_generation), Some(1));
    wait_until(&harness.reader, "svc", |s| {
        s.run_generation == 2 && s.execution == Execution::Running
    })
    .await?;

    // A strict (acknowledged) restart of a disabled service is rejected, not a silent re-enable.
    accepted(harness.control.disable(&id).await)?;
    wait_until(&harness.reader, "svc", |s| s.desired == Desired::Disabled).await?;
    let rejected = harness
        .control
        .restart(&id)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(matches!(rejected, Err(CommandRejection::InvalidState)));

    // Enable is the operation that starts a disabled service.
    accepted(harness.control.enable(&id).await)?;
    wait_until(&harness.reader, "svc", |s| {
        s.desired == Desired::Enabled && s.execution == Execution::Running
    })
    .await?;

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn commands_do_not_depend_on_event_subscribers() -> eyre::Result<()> {
    let mut services: ServiceMap = ServiceMap::new();
    services.insert(
        "svc".to_string(),
        Service::new(
            "svc",
            Path::new("."),
            service_config("svc", ("sh", &["-c", "sleep 60"])),
        )?,
    );
    let harness = spawn_harness(services);
    let id = "svc".to_string();

    wait_until(&harness.reader, "svc", |s| {
        s.execution == Execution::Running
    })
    .await?;
    // Acknowledged commands round-trip and advance the model without any event-channel
    // subscriber; the model is the scheduler's only runtime publication path.
    let acks = accepted(harness.control.restart(&id).await)?;
    assert_eq!(acks.first().map(|a| a.observed_generation), Some(1));
    wait_until(&harness.reader, "svc", |s| s.run_generation == 2).await?;

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

fn reload_test_yaml(message: &str) -> String {
    format!(
        r#"version: "1"
services:
  svc:
    command: ["sh", "-c", "echo {message}; sleep 60"]
"#
    )
}

fn auto_reload_test_yaml(command: &str) -> eyre::Result<String> {
    let command = serde_json::to_string(command)?;
    Ok(format!(
        r#"version: "1"
services:
  svc:
    command: ["sh", "-c", {command}]
    restart: always
"#
    ))
}

fn reload_two_service_yaml(b_command: &str, b_port: u16) -> eyre::Result<String> {
    let b_command = serde_json::to_string(b_command)?;
    Ok(format!(
        r#"version: "1"
services:
  a:
    command: ["sh", "-c", "sleep 60"]
  b:
    command: ["sh", "-c", {b_command}]
    ports: [{b_port}]
"#
    ))
}

fn services_from_config_path(config_path: &Path) -> eyre::Result<ServiceMap> {
    let raw = fs::read_to_string(config_path)?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| eyre::eyre!("missing config parent"))?;
    let mut diagnostics = Vec::new();
    let config = crate::config::from_str(&raw, config_dir, 0usize, None, &mut diagnostics)?;
    crate::service_map_from_config(&config)
}

#[test]
fn reload_re_reads_file_strict_mode() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("micromux.yaml");
    let yaml = |strict| {
        format!(
            r#"version: "1"
strict: {strict}
services:
  svc:
    command: ["sh", "-c", "true"]
    unknown_key: true
"#
        )
    };
    let reload = ReloadConfig {
        config_path: config_path.clone(),
        strict_override: None,
    };

    fs::write(&config_path, yaml("false"))?;
    load_services_from_disk(&reload).map_err(eyre::Report::msg)?;

    fs::write(&config_path, yaml("true"))?;
    let err = load_services_from_disk(&reload).expect_err("strict reload should reject warning");
    assert!(err.contains("unknown service field"));
    Ok(())
}

#[tokio::test]
async fn restart_reloads_latest_service_config_before_spawning() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("micromux.yaml");
    fs::write(&config_path, reload_test_yaml("old-config"))?;
    let services = services_from_config_path(&config_path)?;
    let harness = spawn_harness_with_reload(services, config_path.clone());
    let id = "svc".to_string();

    wait_for_log(&harness.reader, "svc", "old-config").await?;
    fs::write(&config_path, reload_test_yaml("new-config"))?;

    let acks = accepted(harness.control.restart(&id).await)?;
    assert_eq!(acks.first().map(|ack| ack.observed_generation), Some(1));
    wait_until(&harness.reader, "svc", |snapshot| {
        snapshot.run_generation == 2 && snapshot.execution == Execution::Running
    })
    .await?;
    wait_for_log(&harness.reader, "svc", "new-config").await?;

    let logs = harness.reader.logs("svc", None);
    assert!(logs.iter().any(|line| line.line.contains("new-config")));
    assert!(!logs.iter().any(|line| line.line.contains("old-config")));

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn restart_rejects_invalid_reloaded_config_without_killing_current_run() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("micromux.yaml");
    fs::write(&config_path, reload_test_yaml("still-running"))?;
    let services = services_from_config_path(&config_path)?;
    let harness = spawn_harness_with_reload(services, config_path.clone());
    let id = "svc".to_string();

    wait_for_log(&harness.reader, "svc", "still-running").await?;
    let before = wait_until(&harness.reader, "svc", |snapshot| {
        snapshot.run_generation == 1 && snapshot.execution == Execution::Running
    })
    .await?;
    fs::write(
        &config_path,
        r#"version: "1"
services:
  svc:
    working_dir: ./
"#,
    )?;

    let rejected = harness
        .control
        .restart(&id)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(matches!(rejected, Err(CommandRejection::ConfigReload(_))));

    let after = wait_until(&harness.reader, "svc", |snapshot| {
        snapshot.run_generation == 1 && snapshot.execution == Execution::Running
    })
    .await?;
    assert_eq!(after.run_generation, before.run_generation);

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn automatic_restart_reloads_latest_service_config_before_spawning() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("micromux.yaml");
    fs::write(
        &config_path,
        auto_reload_test_yaml("echo old-auto; sleep 1; exit 1")?,
    )?;
    let services = services_from_config_path(&config_path)?;
    let harness = spawn_harness_with_reload(services, config_path.clone());

    wait_for_log(&harness.reader, "svc", "old-auto").await?;
    fs::write(
        &config_path,
        auto_reload_test_yaml("echo new-auto; sleep 60")?,
    )?;

    wait_for_log(&harness.reader, "svc", "new-auto").await?;
    wait_until(&harness.reader, "svc", |snapshot| {
        snapshot.run_generation >= 2 && snapshot.execution == Execution::Running
    })
    .await?;

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn reload_does_not_rewrite_snapshot_for_unrestarted_run() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("micromux.yaml");
    fs::write(
        &config_path,
        reload_two_service_yaml("echo old-b; sleep 60", 3000)?,
    )?;
    let services = services_from_config_path(&config_path)?;
    let harness = spawn_harness_with_reload(services, config_path.clone());
    let a = "a".to_string();
    let b = "b".to_string();

    wait_until(&harness.reader, "a", |snapshot| {
        snapshot.execution == Execution::Running
    })
    .await?;
    let before_b = wait_until(&harness.reader, "b", |snapshot| {
        snapshot.execution == Execution::Running
    })
    .await?;
    assert_eq!(before_b.advertised_ports, vec![3000]);
    assert_eq!(
        before_b.command,
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo old-b; sleep 60".to_string()
        ]
    );

    fs::write(
        &config_path,
        reload_two_service_yaml("echo new-b; sleep 60", 4000)?,
    )?;
    accepted(harness.control.restart(&a).await)?;
    wait_until(&harness.reader, "a", |snapshot| {
        snapshot.run_generation == 2 && snapshot.execution == Execution::Running
    })
    .await?;

    let still_old_b = harness
        .reader
        .service("b")
        .ok_or_else(|| eyre::eyre!("missing b snapshot"))?;
    assert_eq!(still_old_b.advertised_ports, vec![3000]);
    assert_eq!(still_old_b.command, before_b.command);

    accepted(harness.control.restart(&b).await)?;
    let restarted_b = wait_until(&harness.reader, "b", |snapshot| {
        snapshot.run_generation == 2 && snapshot.execution == Execution::Running
    })
    .await?;
    assert_eq!(restarted_b.advertised_ports, vec![4000]);
    assert_eq!(
        restarted_b.command,
        vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo new-b; sleep 60".to_string()
        ]
    );

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

fn spanned_string(value: &str) -> Spanned<String> {
    Spanned {
        span: yaml_spanned::spanned::Span::default(),
        inner: value.to_string(),
    }
}

fn service_config(name: &str, command: (&str, &[&str])) -> config::Service {
    config::Service {
        name: spanned_string(name),
        startup_mode: StartupMode::Enabled,
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
        restart_policy: crate::service::RestartPolicy::Never,
        color: None,
        log_retention: crate::LogRetention::default(),
    }
}

fn unique_tmp_dir(prefix: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("micromux-{prefix}-{nanos}"))
}

async fn recv_event(mut rx: mpsc::Receiver<Event>) -> eyre::Result<(Event, mpsc::Receiver<Event>)> {
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
    let (test_events_tx, mut test_events_rx) = mpsc::channel(128);
    let (events_tx, events_rx) = mpsc::channel(128);
    let (_commands_tx, commands_rx) = mpsc::channel(128);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let mut saw_hc_success = false;
    for _ in 0..200 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
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
    let (test_events_tx, mut test_events_rx) = mpsc::channel(128);
    let (events_tx, events_rx) = mpsc::channel(128);
    let (_commands_tx, commands_rx) = mpsc::channel(128);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let mut saw_hc_success = false;
    for _ in 0..200 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
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
    let (test_events_tx, mut test_events_rx) = mpsc::channel(128);
    let (events_tx, events_rx) = mpsc::channel(128);
    let (_commands_tx, commands_rx) = mpsc::channel(128);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let mut saw_log_line = false;
    let mut saw_finished = false;
    for _ in 0..200 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
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
    let (test_events_tx, test_events_rx) = mpsc::channel(64);
    let (events_tx, events_rx) = mpsc::channel(64);
    let (commands_tx, commands_rx) = mpsc::channel(64);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let (event, mut test_events_rx) = recv_event(test_events_rx).await?;
    assert!(matches!(event, Event::Started { .. }));

    commands_tx
        .send(Command::disable("svc".to_string()))
        .await?;

    loop {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
        if matches!(event, Event::Killed(_) | Event::Exited(_, _)) {
            break;
        }
    }

    shutdown.cancel();
    handle.await??;
    Ok(())
}

#[tokio::test]
async fn shutdown_drains_running_service() -> eyre::Result<()> {
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
    let (test_events_tx, test_events_rx) = mpsc::channel(64);
    let (events_tx, events_rx) = mpsc::channel(64);
    let (_commands_tx, commands_rx) = mpsc::channel(64);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let (event, _test_events_rx) = recv_event(test_events_rx).await?;
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

    shutdown.cancel();

    timeout(Duration::from_secs(3), handle)
        .await
        .map_err(|_| eyre::eyre!("scheduler did not drain after shutdown"))???;

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
    let (test_events_tx, test_events_rx) = mpsc::channel(64);
    let (events_tx, events_rx) = mpsc::channel(64);
    let (commands_tx, commands_rx) = mpsc::channel(64);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let (event, mut test_events_rx) = recv_event(test_events_rx).await?;
    assert!(matches!(event, Event::Started { .. }));

    commands_tx
        .send(Command::restart("svc".to_string()))
        .await?;

    let mut saw_second_start = false;
    for _ in 0..10 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
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
    cfg.restart_policy = crate::service::RestartPolicy::Always;

    let mut services: ServiceMap = ServiceMap::new();
    services.insert("svc".to_string(), Service::new("svc", config_dir, cfg)?);

    let shutdown = CancellationToken::new();
    let (test_events_tx, test_events_rx) = mpsc::channel(256);
    let (events_tx, events_rx) = mpsc::channel(256);
    let (_commands_tx, commands_rx) = mpsc::channel(256);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    // The service exits non-zero immediately and has no healthcheck or chatty neighbors,
    // so only the backoff timer can wake the scheduler to restart it. Seeing a second
    // Started with no manual Restart command proves the timer fires.
    let mut starts = 0;
    let mut test_events_rx = test_events_rx;
    for _ in 0..100 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
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
    cfg.restart_policy = crate::service::RestartPolicy::Always;

    let mut services: ServiceMap = ServiceMap::new();
    services.insert("svc".to_string(), Service::new("svc", &dir, cfg)?);

    let shutdown = CancellationToken::new();
    let (test_events_tx, mut test_events_rx) = mpsc::channel(256);
    let (events_tx, events_rx) = mpsc::channel(256);
    let (_commands_tx, commands_rx) = mpsc::channel(256);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let mut starts = 0;
    let mut saw_second_run = false;
    for _ in 0..100 {
        let event = timeout(Duration::from_secs(5), test_events_rx.recv())
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
        match timeout(Duration::from_millis(100), test_events_rx.recv()).await {
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
async fn pty_append_records_are_lossless_under_load() -> eyre::Result<()> {
    let config_dir = Path::new(".");
    let mut cfg = service_config("svc", ("sh", &["-c", "seq 1 5000"]));
    cfg.log_retention = crate::LogRetention {
        memory: crate::MemoryLogRetention {
            max_lines: crate::LogLimit::Unbounded,
            max_bytes: crate::LogLimit::Unbounded,
        },
        disk: crate::DiskLogRetention::default(),
    };
    let mut services: ServiceMap = ServiceMap::new();
    services.insert("svc".to_string(), Service::new("svc", config_dir, cfg)?);
    let harness = spawn_harness(services);

    wait_until(&harness.reader, "svc", |snapshot| {
        snapshot.execution == Execution::Exited
    })
    .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let logs = loop {
        let logs = harness.reader.logs("svc", None);
        if logs.len() == 5000 {
            break logs;
        }
        if tokio::time::Instant::now() >= deadline {
            eyre::bail!("expected 5000 log lines, got {}", logs.len());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(logs.len(), 5000);
    for (idx, line) in logs.iter().enumerate() {
        assert_eq!(line.line, (idx + 1).to_string());
    }
    assert_eq!(logs.last().map(|line| line.line.as_str()), Some("5000"));

    harness.shutdown.cancel();
    harness.handle.await??;
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
    let (test_events_tx, test_events_rx) = mpsc::channel(128);
    let (events_tx, events_rx) = mpsc::channel(128);
    let (_commands_tx, commands_rx) = mpsc::channel(128);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let run_id = RunId::new(1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline && !pty::log_reader_active(&service_id, run_id) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(pty::log_reader_active(&service_id, run_id));

    let mut test_events_rx = test_events_rx;
    let mut saw_exit = false;
    for _ in 0..20 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
        if matches!(event, Event::Exited(id, 0) if id == service_id) {
            saw_exit = true;
            break;
        }
    }
    assert!(saw_exit);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline && pty::log_reader_active(&service_id, run_id) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !pty::log_reader_active(&service_id, run_id),
        "pty reader stayed alive after the PTY slave closed"
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
    let (test_events_tx, test_events_rx) = mpsc::channel(128);
    let (events_tx, events_rx) = mpsc::channel(128);
    let (commands_tx, commands_rx) = mpsc::channel(128);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let (event, mut test_events_rx) = recv_event(test_events_rx).await?;
    assert!(matches!(event, Event::Started { .. }));

    commands_tx
        .send(Command::disable("svc".to_string()))
        .await?;

    loop {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
        if matches!(event, Event::Exited(_, _)) {
            break;
        }
    }

    commands_tx.send(Command::enable("svc".to_string())).await?;

    let mut restarted = false;
    for _ in 0..20 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
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
    let (test_events_tx, test_events_rx) = mpsc::channel(128);
    let (events_tx, events_rx) = mpsc::channel(128);
    let (commands_tx, commands_rx) = mpsc::channel(128);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let mut test_events_rx = test_events_rx;
    let mut saw_log = false;
    for _ in 0..20 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
        if matches!(event, Event::LogLine { line, .. } if line.contains("first-run")) {
            saw_log = true;
            break;
        }
    }
    assert!(saw_log);

    commands_tx
        .send(Command::disable("svc".to_string()))
        .await?;
    loop {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
        if matches!(event, Event::Exited(_, _)) {
            break;
        }
    }

    commands_tx.send(Command::enable("svc".to_string())).await?;

    let mut restarted = false;
    for _ in 0..20 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
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
async fn restart_disabled_service_does_not_reenable_or_clear_logs() -> eyre::Result<()> {
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
    let (test_events_tx, test_events_rx) = mpsc::channel(128);
    let (events_tx, events_rx) = mpsc::channel(128);
    let (commands_tx, commands_rx) = mpsc::channel(128);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let mut test_events_rx = test_events_rx;
    loop {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
        if matches!(event, Event::Started { .. }) {
            break;
        }
    }

    commands_tx
        .send(Command::disable("svc".to_string()))
        .await?;
    loop {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
        if matches!(event, Event::Exited(_, _)) {
            break;
        }
    }

    commands_tx
        .send(Command::restart("svc".to_string()))
        .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(50), test_events_rx.recv()).await {
            Ok(Some(Event::Started { .. })) => {
                eyre::bail!("restart re-enabled a disabled service");
            }
            Ok(Some(Event::ClearLogs(_))) => {
                eyre::bail!("restart on disabled unexpectedly cleared logs");
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
    let (test_events_tx, test_events_rx) = mpsc::channel(256);
    let (events_tx, events_rx) = mpsc::channel(256);
    let (commands_tx, commands_rx) = mpsc::channel(256);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let mut test_events_rx = test_events_rx;
    let mut started = std::collections::HashSet::new();
    while started.len() < 2 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
        if let Event::Started { service_id } = event {
            started.insert(service_id);
        }
    }

    commands_tx
        .send(Command::disable("disabled".to_string()))
        .await?;
    loop {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
        if matches!(event, Event::Exited(service_id, _) if service_id == "disabled") {
            break;
        }
    }

    commands_tx.send(Command::restart_all()).await?;

    let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
    let mut saw_enabled_restart = false;
    while tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(100), test_events_rx.recv()).await {
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
    let (test_events_tx, test_events_rx) = mpsc::channel(256);
    let (events_tx, events_rx) = mpsc::channel(256);
    let (commands_tx, commands_rx) = mpsc::channel(256);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let (event, mut test_events_rx) = recv_event(test_events_rx).await?;
    assert!(matches!(event, Event::Started { service_id } if service_id == "dep"));

    commands_tx
        .send(Command::disable("dep".to_string()))
        .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_millis(700);
    while tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(100), test_events_rx.recv()).await {
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
    let (test_events_tx, test_events_rx) = mpsc::channel(64);
    let (events_tx, events_rx) = mpsc::channel(64);
    let (_commands_tx, commands_rx) = mpsc::channel(64);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let (_event, mut test_events_rx) = recv_event(test_events_rx).await?;

    let mut saw_hello = false;
    let mut saw_err = false;

    for _ in 0..50 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
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
async fn non_alt_screen_control_sequences_keep_raw_log_records() -> eyre::Result<()> {
    let json = r#"{"timestamp":"2026-07-01T22:22:21.512105Z","level":"INFO","fields":{"message":"connecting to qdrant","connection_uri":"http://localhost:6334"},"target":"airtype_api_service::setup","filename":"services/airtype-api-service/src/setup.rs","line_number":234}"#;
    let command = format!("printf '\\033[2Kbuilding\\r'; printf '%s\\n' '{json}'");
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
    let (test_events_tx, test_events_rx) = mpsc::channel(64);
    let (events_tx, events_rx) = mpsc::channel(64);
    let (_commands_tx, commands_rx) = mpsc::channel(64);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let (_event, mut test_events_rx) = recv_event(test_events_rx).await?;

    let mut saw_json = false;
    for _ in 0..50 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
        match event {
            Event::LogLine { line, .. } if line.contains("connecting to qdrant") => {
                assert_eq!(line, json);
                assert_eq!(line.find('\n'), None);
                saw_json = true;
            }
            Event::Exited(_, _) if saw_json => break,
            _ => {}
        }
    }

    assert!(saw_json);
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
    let (test_events_tx, test_events_rx) = mpsc::channel(64);
    let (events_tx, events_rx) = mpsc::channel(64);
    let (_commands_tx, commands_rx) = mpsc::channel(64);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let (_event, mut test_events_rx) = recv_event(test_events_rx).await?;

    let mut saw_tty = false;
    for _ in 0..20 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
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
    let (test_events_tx, test_events_rx) = mpsc::channel(64);
    let (events_tx, events_rx) = mpsc::channel(64);
    let (commands_tx, commands_rx) = mpsc::channel(64);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let (_event, mut test_events_rx) = recv_event(test_events_rx).await?;

    commands_tx
        .send(Command::SendInput("svc".to_string(), b"hello\r".to_vec()))
        .await?;

    let mut saw = false;
    for _ in 0..30 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
        match event {
            Event::LogLine { line, .. } if line.contains("got:hello") => {
                saw = true;
            }
            Event::Exited(_, _) if saw => break,
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
    let (test_events_tx, test_events_rx) = mpsc::channel(256);
    let (events_tx, events_rx) = mpsc::channel(256);
    let (_commands_tx, commands_rx) = mpsc::channel(256);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let (event, mut test_events_rx) = recv_event(test_events_rx).await?;
    assert!(matches!(event, Event::Started { service_id } if service_id == "dep"));

    let mut saw_app_started = false;
    let mut saw_dep_healthy = false;
    for _ in 0..40 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
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
    let (test_events_tx, test_events_rx) = mpsc::channel(256);
    let (events_tx, events_rx) = mpsc::channel(256);
    let (commands_tx, commands_rx) = mpsc::channel(256);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
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

    let (_event, mut test_events_rx) = recv_event(test_events_rx).await?;

    let mut saw_first = false;
    let mut saw_second = false;
    for _ in 0..80 {
        let (event, next_rx) = recv_event(test_events_rx).await?;
        test_events_rx = next_rx;
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
    let (test_events_tx, mut test_events_rx) = mpsc::channel(64);
    let (events_tx, events_rx) = mpsc::channel(64);
    let (_commands_tx, commands_rx) = mpsc::channel(64);

    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            run_test_scheduler(
                &services,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx,
                shutdown,
            )
            .await
        }
    });

    let mut saw_pwd = false;
    let expected = work_abs.canonicalize()?;

    for _ in 0..50 {
        let ev = timeout(Duration::from_secs(5), test_events_rx.recv())
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
