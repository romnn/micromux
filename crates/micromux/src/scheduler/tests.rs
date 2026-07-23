use super::*;
use crate::config;
use crate::service::Service;
use crate::test_util::{service_config, spanned_string, unique_tmp_dir};
use color_eyre::eyre;
use std::fs;
use std::path::{Path, PathBuf};
use tokio::time::{Duration, timeout};
use yaml_spanned::Spanned;

use crate::RestartPolicy;
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
        config_dir: Path::new(".").to_path_buf(),
        dynamic_policy: DynamicServicesPolicy::default(),
        default_log_retention: crate::LogRetention::default(),
    })
    .await
}

/// A scheduler running against a temp config plus model/control handles.
struct Harness {
    reader: crate::model::SessionModelReader,
    control: ServiceControl,
    commands: mpsc::Sender<Command>,
    shutdown: CancellationToken,
    handle: tokio::task::JoinHandle<eyre::Result<()>>,
}

fn spawn_harness(services: ServiceMap, reload_config: Option<ReloadConfig>) -> Harness {
    spawn_harness_with_policy(
        services,
        reload_config,
        Path::new(".").to_path_buf(),
        DynamicServicesPolicy::default(),
    )
}

fn spawn_harness_with_policy(
    services: ServiceMap,
    reload_config: Option<ReloadConfig>,
    config_dir: PathBuf,
    dynamic_policy: DynamicServicesPolicy,
) -> Harness {
    let (commands_tx, commands_rx) = mpsc::channel(64);
    let (events_tx, events_rx) = mpsc::channel(256);
    let (reader, writer) = crate::model::new(crate::initial_model_entries(&services));
    let control = ServiceControl::new(commands_tx.clone());
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            scheduler(SchedulerInput {
                services,
                reload_config,
                commands_rx,
                events_rx,
                events_tx,
                test_events_tx: None,
                writer,
                shutdown,
                config_dir,
                dynamic_policy,
                default_log_retention: crate::LogRetention::default(),
            })
            .await
        }
    });
    Harness {
        reader,
        control,
        commands: commands_tx,
        shutdown,
        handle,
    }
}

fn dynamic_params(id: &str, command: &[&str]) -> DynamicServiceParams {
    DynamicServiceParams {
        service: id.to_string(),
        spec: crate::PartialServiceSpec {
            command: Some(command.iter().map(|part| (*part).to_string()).collect()),
            ..crate::PartialServiceSpec::default()
        },
        from_service: None,
        extra_args: Vec::new(),
        expires_after: None,
        owner: Some("scheduler-test".to_string()),
        idempotency_key: None,
    }
}

fn enabled_dynamic_policy(root: &Path) -> eyre::Result<DynamicServicesPolicy> {
    Ok(DynamicServicesPolicy {
        enabled: true,
        allowed_working_roots: vec![root.canonicalize()?],
        max_services: 4,
        max_lifetime: Some(Duration::from_mins(1)),
    })
}

fn accepted(
    res: Result<ServiceCommandResult, SchedulerStopped>,
) -> eyre::Result<Vec<ServiceCommandAck>> {
    res.map_err(|_| eyre::eyre!("scheduler stopped"))?
        .map_err(|rejection| eyre::eyre!("unexpected rejection: {rejection}"))
}

fn dynamic_accepted(
    result: Result<DynamicServiceResult, SchedulerStopped>,
) -> eyre::Result<DynamicServiceAck> {
    result
        .map_err(|_| eyre::eyre!("scheduler stopped"))?
        .map_err(|rejection| eyre::eyre!("unexpected rejection: {rejection}"))
}

fn reconcile_accepted(
    result: Result<ReconcileResult, SchedulerStopped>,
) -> eyre::Result<ReconcileReceipt> {
    result
        .map_err(|_| eyre::eyre!("scheduler stopped"))?
        .map_err(|rejection| eyre::eyre!("unexpected rejection: {rejection}"))
}

async fn assert_idempotency_collision(control: &ServiceControl) -> eyre::Result<()> {
    let mut collision = dynamic_params("debug", &["true"]);
    collision.idempotency_key = Some("create-debug".to_string());
    let collision = control
        .start_dynamic(collision)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(matches!(collision, Err(CommandRejection::PolicyDenied(_))));
    Ok(())
}

async fn assert_dynamic_creation_and_idempotency(harness: &Harness) -> eyre::Result<()> {
    let mut params = dynamic_params("debug", &["sh", "-c", "echo dynamic; sleep 60"]);
    params.idempotency_key = Some("create-debug".to_string());

    let created = dynamic_accepted(harness.control.start_dynamic(params.clone()).await)?;
    assert_eq!(created.revision, 1);
    assert_eq!(created.observed_generation, 0);
    wait_until(&harness.reader, "debug", |snapshot| {
        snapshot.execution == Execution::Running && snapshot.run_generation == 1
    })
    .await?;
    wait_for_log(&harness.reader, "debug", "dynamic").await?;

    let replay = dynamic_accepted(harness.control.start_dynamic(params).await)?;
    assert!(replay.idempotent_replay);
    assert_eq!(replay.revision, created.revision);
    assert_eq!(
        harness
            .reader
            .service("debug")
            .map(|snapshot| snapshot.run_generation),
        Some(1)
    );
    assert_idempotency_collision(&harness.control).await?;

    let duplicate = harness
        .control
        .start_dynamic(dynamic_params("debug", &["true"]))
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(matches!(duplicate, Err(CommandRejection::InvalidSpec(_))));
    Ok(())
}

async fn assert_dynamic_retirement(harness: &Harness) -> eyre::Result<()> {
    let stopped = dynamic_accepted(harness.control.stop_dynamic(&"debug".to_string()).await)?;
    assert!(!stopped.already_retired);
    let retired = wait_until(&harness.reader, "debug", |snapshot| {
        snapshot.retired == Some(RetiredReason::Stopped)
    })
    .await?;
    assert_eq!(retired.desired, Desired::Disabled);
    assert!(!harness.reader.logs("debug", None).is_empty());
    let stopped_again = dynamic_accepted(harness.control.stop_dynamic(&"debug".to_string()).await)?;
    assert!(stopped_again.already_retired);

    let mut blocked = dynamic_params("blocked", &["true"]);
    blocked.spec.depends_on = Some(vec![crate::DependencySpec {
        service: "debug".to_string(),
        condition: config::DependencyCondition::Started,
    }]);
    let blocked = harness
        .control
        .start_dynamic(blocked)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(matches!(
        blocked,
        Err(CommandRejection::InvalidSpec(message))
            if message.contains("depends on retired service")
                && message.contains("replace_dynamic_service")
    ));
    let restart = harness
        .control
        .restart(&"debug".to_string())
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(matches!(restart, Err(CommandRejection::InvalidState(_))));

    let events = harness.reader.events("debug", None, None).0;
    for kind in [
        ServiceEventKind::Created,
        ServiceEventKind::Replaced,
        ServiceEventKind::Retired,
    ] {
        assert!(events.iter().any(|event| event.kind == kind));
    }
    Ok(())
}

async fn assert_incomplete_replacement_is_non_mutating(harness: &Harness) -> eyre::Result<()> {
    let before = harness
        .reader
        .service("debug")
        .ok_or_else(|| eyre::eyre!("debug service is missing"))?;
    let mut incomplete = dynamic_params("debug", &["true"]);
    incomplete.spec.command = None;

    let result = harness
        .control
        .replace_dynamic(&"debug".to_string(), 1, incomplete)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;

    assert!(matches!(result, Err(CommandRejection::InvalidSpec(_))));
    let after = harness
        .reader
        .service("debug")
        .ok_or_else(|| eyre::eyre!("debug service disappeared"))?;
    assert_eq!(after.run_generation, before.run_generation);
    assert_eq!(
        after.dynamic.map(|dynamic| dynamic.revision),
        before.dynamic.map(|dynamic| dynamic.revision)
    );
    Ok(())
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

async fn wait_for_finished_health_attempt(
    reader: &crate::model::SessionModelReader,
    id: &str,
) -> eyre::Result<crate::model::HealthAttempt> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(attempt) = reader
            .healthchecks(id)
            .into_iter()
            .rev()
            .find(|attempt| attempt.result.is_some())
        {
            return Ok(attempt);
        }
        if tokio::time::Instant::now() >= deadline {
            eyre::bail!("timed out waiting for a finished healthcheck on `{id}`");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[test]
fn effective_lifetime_clamps_requests_to_the_policy_cap() {
    let cap = Duration::from_mins(1);
    // Omitted or "none" fall to the policy default (the cap, or unbounded when uncapped).
    assert_eq!(effective_lifetime(Some(cap), None), Some(cap));
    assert_eq!(
        effective_lifetime(Some(cap), Some(Lease::Unbounded)),
        Some(cap)
    );
    assert_eq!(effective_lifetime(None, None), None);
    assert_eq!(effective_lifetime(None, Some(Lease::Unbounded)), None);
    // Bounded requests: clamped to the cap, honored under it, honored verbatim when uncapped.
    assert_eq!(
        effective_lifetime(Some(cap), Some(Lease::After(Duration::from_hours(1)))),
        Some(cap)
    );
    assert_eq!(
        effective_lifetime(Some(cap), Some(Lease::After(Duration::from_secs(30)))),
        Some(Duration::from_secs(30))
    );
    assert_eq!(
        effective_lifetime(None, Some(Lease::After(Duration::from_secs(30)))),
        Some(Duration::from_secs(30))
    );
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
fn roster_entry_state_classifies_origin_and_retirement() -> eyre::Result<()> {
    let configured = Service::new(
        "configured",
        Path::new("."),
        service_config("configured", ("true", &[])),
    )?;
    let mut dynamic = Service::new(
        "dynamic",
        Path::new("."),
        service_config("dynamic", ("true", &[])),
    )?;
    dynamic.origin = ServiceOrigin::Dynamic(DynamicOrigin {
        created_at_unix_ms: 1,
        expires_at_unix_ms: None,
        owner: None,
        revision: 1,
    });
    let mut runtimes = HashMap::from([
        (
            configured.id.clone(),
            ServiceRuntime::new(ServiceRuntimeInit::from(&configured)),
        ),
        (
            dynamic.id.clone(),
            ServiceRuntime::new(ServiceRuntimeInit::from(&dynamic)),
        ),
    ]);

    assert_eq!(
        roster_entry_state(&runtimes, &configured.id, &configured),
        Some(RosterEntryState::LiveConfigured)
    );
    assert_eq!(
        roster_entry_state(&runtimes, &dynamic.id, &dynamic),
        Some(RosterEntryState::LiveDynamic)
    );

    if let Some(runtime) = runtimes.get_mut(&configured.id) {
        runtime.retired = Some(RetiredReason::Removed);
    }
    if let Some(runtime) = runtimes.get_mut(&dynamic.id) {
        runtime.retired = Some(RetiredReason::Stopped);
    }
    assert_eq!(
        roster_entry_state(&runtimes, &configured.id, &configured),
        Some(RosterEntryState::RetiredConfigured)
    );
    assert_eq!(
        roster_entry_state(&runtimes, &dynamic.id, &dynamic),
        Some(RosterEntryState::RetiredDynamic)
    );
    let missing = "missing".to_string();
    assert_eq!(roster_entry_state(&runtimes, &missing, &configured), None);
    Ok(())
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

#[test]
fn project_snapshot_reports_runtime_identity_and_config_drift() -> eyre::Result<()> {
    let mut service = Service::new(
        "svc",
        Path::new("."),
        service_config("svc", ("sh", &["-c", "sleep 60"])),
    )?;
    let policy = service.spec.restart.clone();
    let mut runtime = ServiceRuntime::new(ServiceRuntimeInit::from(&service));
    runtime.run_config = Some(RunConfig::from(&service));
    start_dummy_run(&mut runtime)?;

    let (running, _) = project_snapshot(&service, &runtime);
    assert_eq!(running.pid, Some(42));
    assert!(running.started_at_unix_ms.is_some());
    assert!(!running.config_stale);

    service = Service::new(
        "svc",
        Path::new("."),
        service_config("svc", ("sh", &["-c", "sleep 60"])),
    )?;
    let (unchanged_reload, _) = project_snapshot(&service, &runtime);
    assert!(!unchanged_reload.config_stale);

    service.spec.command = vec!["false".to_string()];
    let (stale, _) = project_snapshot(&service, &runtime);
    assert!(stale.config_stale);
    assert_eq!(stale.command, vec!["sh", "-c", "sleep 60"]);

    runtime.finish_current_run(&policy, 0);
    let (exited, uptime_started_at) = project_snapshot(&service, &runtime);
    assert_eq!(exited.pid, None);
    assert_eq!(exited.started_at_unix_ms, None);
    assert_eq!(uptime_started_at, None);
    Ok(())
}

#[test]
fn project_snapshot_reports_active_restart_backoff() -> eyre::Result<()> {
    let mut config = service_config("svc", ("false", &[]));
    config.restart_policy = crate::service::RestartPolicy::Always;
    let service = Service::new("svc", Path::new("."), config)?;
    let mut runtime = ServiceRuntime::new(ServiceRuntimeInit::from(&service));
    runtime.run_config = Some(RunConfig::from(&service));
    start_dummy_run(&mut runtime)?;
    runtime.finish_current_run(&service.spec.restart, 1);

    let (snapshot, _) = project_snapshot(&service, &runtime);

    let restart = snapshot
        .restart_state
        .ok_or_else(|| eyre::eyre!("missing active restart state"))?;
    assert_eq!(restart.backoff_delay, RESTART_BACKOFF_BASE);
    assert_eq!(restart.restarts_remaining, None);
    Ok(())
}

/// Start a fake run on `runtime` without spawning a process, for handle-bookkeeping tests.
fn start_dummy_run(runtime: &mut ServiceRuntime) -> eyre::Result<RunId> {
    runtime.mark_starting();
    let run_id = runtime.allocate_run_id();
    runtime.mark_started(RunningService {
        run_id,
        pid: Some(42),
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
    let harness = spawn_harness(services, None);
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
    assert!(matches!(rejected, Err(CommandRejection::InvalidState(_))));

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
    let harness = spawn_harness(services, None);
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
    assert!(matches!(rejected, Err(CommandRejection::InvalidState(_))));

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
async fn dynamic_service_create_replace_stop_and_idempotency() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let harness = spawn_harness_with_policy(
        ServiceMap::new(),
        None,
        dir.path().to_path_buf(),
        enabled_dynamic_policy(dir.path())?,
    );
    assert_dynamic_creation_and_idempotency(&harness).await?;
    assert_incomplete_replacement_is_non_mutating(&harness).await?;

    let stale = harness
        .control
        .replace_dynamic(
            &"debug".to_string(),
            0,
            dynamic_params("debug", &["sh", "-c", "sleep 60"]),
        )
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(matches!(
        stale,
        Err(CommandRejection::RevisionMismatch {
            expected: 0,
            actual: 1
        })
    ));

    let replaced = dynamic_accepted(
        harness
            .control
            .replace_dynamic(
                &"debug".to_string(),
                1,
                dynamic_params("debug", &["sh", "-c", "echo replaced; sleep 60"]),
            )
            .await,
    )?;
    assert_eq!(replaced.revision, 2);
    assert_eq!(replaced.observed_generation, 1);
    wait_until(&harness.reader, "debug", |snapshot| {
        snapshot.execution == Execution::Running && snapshot.run_generation == 2
    })
    .await?;
    wait_for_log(&harness.reader, "debug", "replaced").await?;

    assert_dynamic_retirement(&harness).await?;

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn self_referential_dynamic_replace_overlays_and_reappends_extra_args() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let harness = spawn_harness_with_policy(
        ServiceMap::new(),
        None,
        dir.path().to_path_buf(),
        enabled_dynamic_policy(dir.path())?,
    );
    let id = "debug".to_string();
    let created = dynamic_accepted(
        harness
            .control
            .start_dynamic(dynamic_params(&id, &["sh", "-c", "sleep 60"]))
            .await,
    )?;
    assert_eq!(created.command, vec!["sh", "-c", "sleep 60"]);

    let mut replacement = dynamic_params(&id, &["unused"]);
    replacement.spec.command = None;
    replacement.spec.restart = Some(RestartPolicy::Always);
    replacement.from_service = Some(id.clone());
    replacement.extra_args = vec!["--trace".to_string()];
    let replaced = dynamic_accepted(
        harness
            .control
            .replace_dynamic(&id, 1, replacement.clone())
            .await,
    )?;
    assert_eq!(replaced.revision, 2);
    assert_eq!(replaced.restart, RestartPolicy::Always);
    assert_eq!(replaced.command, vec!["sh", "-c", "sleep 60", "--trace"]);

    let repeated = dynamic_accepted(harness.control.replace_dynamic(&id, 2, replacement).await)?;
    assert_eq!(repeated.revision, 3);
    assert_eq!(repeated.restart, RestartPolicy::Always);
    assert_eq!(
        repeated.command,
        vec!["sh", "-c", "sleep 60", "--trace", "--trace"]
    );

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn dynamic_policy_limit_ttl_and_dependency_gating() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut dep_config = service_config("dep", ("sh", &["-c", "sleep 60"]));
    dep_config.startup_mode = StartupMode::Disabled;
    let mut services = ServiceMap::new();
    services.insert(
        "dep".to_string(),
        Service::new("dep", dir.path(), dep_config)?,
    );
    let mut policy = enabled_dynamic_policy(dir.path())?;
    policy.max_services = 1;
    policy.max_lifetime = Some(Duration::from_millis(250));
    let harness = spawn_harness_with_policy(services, None, dir.path().to_path_buf(), policy);
    let mut params = dynamic_params("worker", &["sh", "-c", "sleep 60"]);
    params.expires_after = Some(Lease::Unbounded);
    params.spec.depends_on = Some(vec![crate::DependencySpec {
        service: "dep".to_string(),
        condition: config::DependencyCondition::Started,
    }]);
    let created = dynamic_accepted(harness.control.start_dynamic(params).await)?;
    let now = unix_now_ms().unwrap_or_default();
    assert!(
        created
            .expires_at_unix_ms
            .is_some_and(|expires_at| expires_at.saturating_sub(now) <= 250)
    );
    let blocked = wait_until(&harness.reader, "worker", |snapshot| {
        snapshot.execution == Execution::Pending
    })
    .await?;
    assert_eq!(blocked.run_generation, 0);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let blocked_events = harness
        .reader
        .events("worker", None, None)
        .0
        .into_iter()
        .filter(|event| event.kind == ServiceEventKind::DependencyBlocked)
        .count();
    assert_eq!(blocked_events, 1);

    let limited = harness
        .control
        .start_dynamic(dynamic_params("other", &["true"]))
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(matches!(limited, Err(CommandRejection::LimitExceeded(_))));
    // A merely *disabled* dynamic still holds its slot; only retirement or expiry frees it.
    accepted(harness.control.disable(&"worker".to_string()).await)?;
    let limited = harness
        .control
        .start_dynamic(dynamic_params("other", &["true"]))
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(matches!(limited, Err(CommandRejection::LimitExceeded(_))));
    accepted(harness.control.enable(&"worker".to_string()).await)?;
    accepted(harness.control.enable(&"dep".to_string()).await)?;
    wait_until(&harness.reader, "worker", |snapshot| {
        snapshot.execution == Execution::Running
    })
    .await?;
    wait_until(&harness.reader, "worker", |snapshot| {
        snapshot.retired == Some(RetiredReason::Expired)
    })
    .await?;
    let events = harness.reader.events("worker", None, None).0;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == ServiceEventKind::DependencyBlocked)
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|event| event.kind == ServiceEventKind::DependencyReady)
    );

    dynamic_accepted(
        harness
            .control
            .start_dynamic(dynamic_params("other", &["sh", "-c", "sleep 60"]))
            .await,
    )?;

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn dynamic_lifetime_overflow_is_rejected_without_mutating_the_roster() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut policy = enabled_dynamic_policy(dir.path())?;
    policy.max_lifetime = Some(Duration::MAX);
    let harness =
        spawn_harness_with_policy(ServiceMap::new(), None, dir.path().to_path_buf(), policy);

    let result = harness
        .control
        .start_dynamic(dynamic_params("too-long", &["true"]))
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;

    assert!(
        matches!(result, Err(CommandRejection::InvalidSpec(message)) if message.contains("monotonic clock"))
    );
    assert!(harness.reader.service("too-long").is_none());

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn unbounded_dynamic_lease_has_no_expiry_and_does_not_retire() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut policy = enabled_dynamic_policy(dir.path())?;
    policy.max_lifetime = None;
    let harness =
        spawn_harness_with_policy(ServiceMap::new(), None, dir.path().to_path_buf(), policy);
    let mut params = dynamic_params("unbounded", &["sh", "-c", "sleep 60"]);
    params.expires_after = Some(Lease::Unbounded);

    let receipt = dynamic_accepted(harness.control.start_dynamic(params).await)?;
    assert_eq!(receipt.expires_at_unix_ms, None);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let snapshot = harness
        .reader
        .service("unbounded")
        .ok_or_else(|| eyre::eyre!("unbounded service is missing"))?;
    let dynamic = snapshot
        .dynamic
        .ok_or_else(|| eyre::eyre!("unbounded service has no dynamic metadata"))?;
    assert_eq!(dynamic.expires_at_unix_ms, None);
    assert_eq!(snapshot.retired, None);

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn renewing_dynamic_lease_preserves_the_running_process_and_revision() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let harness = spawn_harness_with_policy(
        ServiceMap::new(),
        None,
        dir.path().to_path_buf(),
        enabled_dynamic_policy(dir.path())?,
    );
    let mut params = dynamic_params("renewable", &["sh", "-c", "sleep 60"]);
    params.expires_after = Some(Lease::After(Duration::from_secs(10)));
    dynamic_accepted(harness.control.start_dynamic(params).await)?;
    let before = wait_until(&harness.reader, "renewable", |snapshot| {
        snapshot.execution == Execution::Running
    })
    .await?;
    let before_expiry = before
        .dynamic
        .as_ref()
        .and_then(|dynamic| dynamic.expires_at_unix_ms)
        .ok_or_else(|| eyre::eyre!("bounded lease has no expiry"))?;

    let stale = harness
        .control
        .renew_dynamic(&"renewable".to_string(), 0, None)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(matches!(
        stale,
        Err(CommandRejection::RevisionMismatch {
            expected: 0,
            actual: 1
        })
    ));

    let receipt = dynamic_accepted(
        harness
            .control
            .renew_dynamic(&"renewable".to_string(), 1, None)
            .await,
    )?;
    let after = harness
        .reader
        .service("renewable")
        .ok_or_else(|| eyre::eyre!("renewable service is missing"))?;
    let after_dynamic = after
        .dynamic
        .as_ref()
        .ok_or_else(|| eyre::eyre!("renewable service has no dynamic metadata"))?;
    assert_eq!(receipt.revision, 1);
    assert_eq!(receipt.observed_generation, before.run_generation);
    assert_eq!(after.run_generation, before.run_generation);
    assert_eq!(after.pid, before.pid);
    assert_eq!(after_dynamic.revision, 1);
    assert!(
        after_dynamic
            .expires_at_unix_ms
            .is_some_and(|expiry| expiry > before_expiry)
    );
    assert!(
        harness
            .reader
            .events("renewable", None, None)
            .0
            .iter()
            .any(|event| event.kind == ServiceEventKind::LeaseRenewed)
    );

    dynamic_accepted(harness.control.stop_dynamic(&"renewable".to_string()).await)?;
    let retired = harness
        .control
        .renew_dynamic(&"renewable".to_string(), 1, None)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(
        matches!(retired, Err(CommandRejection::InvalidState(message)) if message.contains("replace_dynamic_service"))
    );

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn renewing_to_unbounded_clears_the_expiry_deadline() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut policy = enabled_dynamic_policy(dir.path())?;
    policy.max_lifetime = None;
    let harness =
        spawn_harness_with_policy(ServiceMap::new(), None, dir.path().to_path_buf(), policy);
    let mut params = dynamic_params("renewable", &["sh", "-c", "sleep 60"]);
    params.expires_after = Some(Lease::After(Duration::from_secs(10)));
    dynamic_accepted(harness.control.start_dynamic(params).await)?;
    let before = harness
        .reader
        .service("renewable")
        .and_then(|snapshot| snapshot.dynamic)
        .and_then(|dynamic| dynamic.expires_at_unix_ms);
    assert!(before.is_some());

    let receipt = dynamic_accepted(
        harness
            .control
            .renew_dynamic(&"renewable".to_string(), 1, Some(Lease::Unbounded))
            .await,
    )?;
    assert_eq!(receipt.expires_at_unix_ms, None);
    let after = harness
        .reader
        .service("renewable")
        .and_then(|snapshot| snapshot.dynamic)
        .and_then(|dynamic| dynamic.expires_at_unix_ms);
    assert_eq!(after, None);

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn reviving_retired_dynamic_requires_a_free_live_slot() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut policy = enabled_dynamic_policy(dir.path())?;
    policy.max_services = 1;
    let harness =
        spawn_harness_with_policy(ServiceMap::new(), None, dir.path().to_path_buf(), policy);

    dynamic_accepted(
        harness
            .control
            .start_dynamic(dynamic_params("retired", &["sh", "-c", "sleep 60"]))
            .await,
    )?;
    dynamic_accepted(harness.control.stop_dynamic(&"retired".to_string()).await)?;
    dynamic_accepted(
        harness
            .control
            .start_dynamic(dynamic_params("occupant", &["sh", "-c", "sleep 60"]))
            .await,
    )?;

    let replacement = harness
        .control
        .replace_dynamic(
            &"retired".to_string(),
            1,
            dynamic_params("retired", &["sh", "-c", "sleep 60"]),
        )
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(matches!(
        replacement,
        Err(CommandRejection::LimitExceeded(_))
    ));
    assert!(harness.reader.service("retired").is_some_and(|snapshot| {
        snapshot
            .dynamic
            .is_some_and(|dynamic| dynamic.revision == 1)
            && snapshot.retired == Some(RetiredReason::Stopped)
    }));

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn dynamic_working_root_rejects_symlink_escape() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let allowed = dir.path().join("allowed");
    let outside = dir.path().join("outside");
    fs::create_dir_all(&allowed)?;
    fs::create_dir_all(&outside)?;
    std::os::unix::fs::symlink(&outside, allowed.join("escape"))?;
    let harness = spawn_harness_with_policy(
        ServiceMap::new(),
        None,
        dir.path().to_path_buf(),
        enabled_dynamic_policy(&allowed)?,
    );
    let mut params = dynamic_params("escaped", &["true"]);
    params.spec.working_dir = crate::SpecField::Value(PathBuf::from("allowed/escape"));

    let result = harness
        .control
        .start_dynamic(params)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(
        matches!(result, Err(CommandRejection::PolicyDenied(message)) if message.contains("allowed_working_roots"))
    );
    assert!(harness.reader.service("escaped").is_none());

    // A plain `..` traversal must be caught by the same canonicalization.
    let mut params = dynamic_params("dotdot", &["true"]);
    params.spec.working_dir = crate::SpecField::Value(PathBuf::from("allowed/../outside"));
    let result = harness
        .control
        .start_dynamic(params)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(
        matches!(result, Err(CommandRejection::PolicyDenied(message)) if message.contains("allowed_working_roots"))
    );
    assert!(harness.reader.service("dotdot").is_none());

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn dynamic_replace_rejects_cycles_before_mutating() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let harness = spawn_harness_with_policy(
        ServiceMap::new(),
        None,
        dir.path().to_path_buf(),
        enabled_dynamic_policy(dir.path())?,
    );
    dynamic_accepted(
        harness
            .control
            .start_dynamic(dynamic_params("a", &["sh", "-c", "sleep 60"]))
            .await,
    )?;
    let mut b = dynamic_params("b", &["sh", "-c", "sleep 60"]);
    b.spec.depends_on = Some(vec![crate::DependencySpec {
        service: "a".to_string(),
        condition: config::DependencyCondition::Started,
    }]);
    dynamic_accepted(harness.control.start_dynamic(b).await)?;
    let before = wait_until(&harness.reader, "a", |snapshot| {
        snapshot.execution == Execution::Running
    })
    .await?;

    let mut replacement = dynamic_params("a", &["false"]);
    replacement.spec.depends_on = Some(vec![crate::DependencySpec {
        service: "b".to_string(),
        condition: config::DependencyCondition::Started,
    }]);
    let result = harness
        .control
        .replace_dynamic(&"a".to_string(), 1, replacement)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(matches!(result, Err(CommandRejection::InvalidSpec(_))));
    let after = harness
        .reader
        .service("a")
        .ok_or_else(|| eyre::eyre!("service a disappeared"))?;
    assert_eq!(after.run_generation, before.run_generation);
    assert_eq!(after.command, before.command);
    assert_eq!(after.dynamic.map(|origin| origin.revision), Some(1));

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn retired_eviction_keeps_live_dependency_targets() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut policy = enabled_dynamic_policy(dir.path())?;
    policy.max_services = MAX_RETIRED_SERVICES + 4;
    let harness =
        spawn_harness_with_policy(ServiceMap::new(), None, dir.path().to_path_buf(), policy);

    dynamic_accepted(
        harness
            .control
            .start_dynamic(dynamic_params("anchor", &["sh", "-c", "sleep 60"]))
            .await,
    )?;
    wait_until(&harness.reader, "anchor", |snapshot| {
        snapshot.execution == Execution::Running
    })
    .await?;

    let mut dependent = dynamic_params("dependent", &["sh", "-c", "sleep 60"]);
    dependent.spec.depends_on = Some(vec![crate::DependencySpec {
        service: "anchor".to_string(),
        condition: config::DependencyCondition::Started,
    }]);
    dynamic_accepted(harness.control.start_dynamic(dependent).await)?;
    wait_until(&harness.reader, "dependent", |snapshot| {
        snapshot.execution == Execution::Running
    })
    .await?;
    dynamic_accepted(harness.control.stop_dynamic(&"anchor".to_string()).await)?;

    for index in 0..(MAX_RETIRED_SERVICES + 2) {
        let id = format!("disposable-{index:02}");
        dynamic_accepted(
            harness
                .control
                .start_dynamic(dynamic_params(&id, &["true"]))
                .await,
        )?;
        wait_until(&harness.reader, &id, |snapshot| {
            snapshot.execution == Execution::Exited
        })
        .await?;
        dynamic_accepted(harness.control.stop_dynamic(&id).await)?;
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let snapshots = harness.reader.services();
        let retired = snapshots
            .iter()
            .filter(|snapshot| snapshot.retired.is_some())
            .count();
        if retired <= MAX_RETIRED_SERVICES {
            assert!(snapshots.iter().any(|snapshot| snapshot.id == "anchor"));
            assert!(snapshots.iter().any(|snapshot| snapshot.id == "dependent"));
            // Eviction removes the *oldest* evictable entries: the first disposable goes even
            // though the (older) depended-upon anchor stays, and the newest disposable survives.
            assert!(
                !snapshots
                    .iter()
                    .any(|snapshot| snapshot.id == "disposable-00")
            );
            let newest = format!("disposable-{:02}", MAX_RETIRED_SERVICES + 1);
            assert!(snapshots.iter().any(|snapshot| snapshot.id == newest));
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            eyre::bail!("retired dynamic roster did not return to its bounded size");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn retired_eviction_waits_for_slow_processes_and_log_readers() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let mut policy = enabled_dynamic_policy(dir.path())?;
    policy.max_services = MAX_RETIRED_SERVICES + 2;
    let harness =
        spawn_harness_with_policy(ServiceMap::new(), None, dir.path().to_path_buf(), policy);
    let retired_ids = (0..=MAX_RETIRED_SERVICES)
        .map(|index| format!("slow-{index:02}"))
        .collect::<Vec<_>>();

    for id in &retired_ids {
        dynamic_accepted(
            harness
                .control
                .start_dynamic(dynamic_params(
                    id,
                    &["sh", "-c", "trap '' TERM; echo ready; sleep 60"],
                ))
                .await,
        )?;
        wait_for_log(&harness.reader, id, "ready").await?;
    }
    for id in &retired_ids {
        dynamic_accepted(harness.control.stop_dynamic(id).await)?;
    }

    dynamic_accepted(
        harness
            .control
            .start_dynamic(dynamic_params("trigger", &["sh", "-c", "sleep 60"]))
            .await,
    )?;

    let snapshots = harness.reader.services();
    for id in &retired_ids {
        assert!(
            snapshots.iter().any(|snapshot| &snapshot.id == id),
            "still-draining retired service `{id}` was evicted"
        );
    }

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn dynamic_services_are_denied_by_default() -> eyre::Result<()> {
    let harness = spawn_harness(ServiceMap::new(), None);
    let result = harness
        .control
        .start_dynamic(dynamic_params("debug", &["true"]))
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(matches!(result, Err(CommandRejection::PolicyDenied(_))));
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
    let harness = spawn_harness(services, None);
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

async fn assert_reconcile_dry_run(
    harness: &Harness,
    before_x: &crate::model::ServiceSnapshot,
    before_y: &crate::model::ServiceSnapshot,
) -> eyre::Result<ReconcileReceipt> {
    let dry_run = reconcile_accepted(harness.control.reconcile_config(true).await)?;
    assert!(dry_run.dry_run);
    assert_eq!(
        dry_run
            .actions
            .iter()
            .map(|action| (action.service.as_str(), action.action))
            .collect::<Vec<_>>(),
        vec![
            ("x", ReconcileActionKind::Removed),
            ("y", ReconcileActionKind::Changed),
            ("z", ReconcileActionKind::Added),
        ]
    );
    let changed = dry_run
        .actions
        .iter()
        .find(|action| action.service == "y")
        .ok_or_else(|| eyre::eyre!("missing y action"))?;
    for component in ["service spec", "startup mode", "log retention"] {
        assert!(changed.detail.contains(component), "{}", changed.detail);
    }
    assert_eq!(
        harness.reader.service("x").map(|snapshot| (
            snapshot.run_generation,
            snapshot.command,
            snapshot.retired,
        )),
        Some((before_x.run_generation, before_x.command.clone(), None))
    );
    assert_eq!(
        harness.reader.service("y").map(|snapshot| (
            snapshot.run_generation,
            snapshot.command,
            snapshot.retired,
        )),
        Some((before_y.run_generation, before_y.command.clone(), None))
    );
    assert!(harness.reader.service("z").is_none());
    Ok(dry_run)
}

async fn apply_reconcile_and_assert(
    harness: &Harness,
    config_path: &Path,
    before_x: &crate::model::ServiceSnapshot,
    before_y: &crate::model::ServiceSnapshot,
    dry_run: &ReconcileReceipt,
) -> eyre::Result<()> {
    let applied = reconcile_accepted(harness.control.reconcile_config(false).await)?;
    assert!(!applied.dry_run);
    assert_eq!(applied.actions, dry_run.actions);
    let removed = wait_until(&harness.reader, "x", |snapshot| {
        snapshot.retired == Some(RetiredReason::Removed)
    })
    .await?;
    assert_eq!(removed.desired, Desired::Disabled);
    assert!(removed.retired_at_unix_ms.is_some());
    assert!(
        harness
            .reader
            .logs("x", None)
            .iter()
            .any(|line| line.line.contains("x-old"))
    );
    wait_for_log(&harness.reader, "z", "z-new").await?;
    let stale_y = harness
        .reader
        .service("y")
        .ok_or_else(|| eyre::eyre!("missing y after reconcile"))?;
    assert_eq!(stale_y.run_generation, before_y.run_generation);
    assert_eq!(stale_y.pid, before_y.pid);
    assert!(stale_y.config_stale);
    assert_eq!(stale_y.command, before_y.command);

    accepted(harness.control.restart(&"y".to_string()).await)?;
    wait_until(&harness.reader, "y", |snapshot| {
        snapshot.run_generation == before_y.run_generation + 1
            && snapshot.execution == Execution::Running
    })
    .await?;
    wait_for_log(&harness.reader, "y", "y-new").await?;

    fs::write(
        config_path,
        r#"version: 1
services:
  x:
    command: ["sh", "-c", "echo x-revived; sleep 60"]
  y:
    command: ["sh", "-c", "echo y-new; sleep 60"]
    disabled: true
    logs:
      retained_runs: 2
  z:
    command: ["sh", "-c", "echo z-new; sleep 60"]
"#,
    )?;
    let implicit_revival = harness
        .control
        .restart(&"y".to_string())
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(
        matches!(implicit_revival, Err(CommandRejection::ConfigReload(message)) if message.contains("reconcile_config"))
    );
    let revived = reconcile_accepted(harness.control.reconcile_config(false).await)?;
    let [revived_action] = revived.actions.as_slice() else {
        eyre::bail!("expected one revival action, got {:?}", revived.actions);
    };
    assert_eq!(revived_action.service, "x");
    assert_eq!(revived_action.action, ReconcileActionKind::Added);
    let revived_x = wait_until(&harness.reader, "x", |snapshot| {
        snapshot.retired.is_none()
            && snapshot.run_generation == before_x.run_generation + 1
            && snapshot.execution == Execution::Running
    })
    .await?;
    assert_eq!(revived_x.origin, OriginKind::Configured);
    wait_for_log(&harness.reader, "x", "x-revived").await
}

#[tokio::test]
async fn reconcile_dry_run_and_apply_cover_add_remove_change_reload_and_revival() -> eyre::Result<()>
{
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("micromux.yaml");
    fs::write(
        &config_path,
        r#"version: 1
services:
  x:
    command: ["sh", "-c", "echo x-old; sleep 60"]
  y:
    command: ["sh", "-c", "echo y-old; sleep 60"]
"#,
    )?;
    let services = services_from_config_path(&config_path)?;
    let harness = spawn_harness(
        services,
        Some(ReloadConfig {
            config_path: config_path.clone(),
            strict_override: None,
        }),
    );
    wait_for_log(&harness.reader, "x", "x-old").await?;
    wait_for_log(&harness.reader, "y", "y-old").await?;
    let before_x = harness
        .reader
        .service("x")
        .ok_or_else(|| eyre::eyre!("missing x"))?;
    let before_y = harness
        .reader
        .service("y")
        .ok_or_else(|| eyre::eyre!("missing y"))?;

    fs::write(
        &config_path,
        r#"version: 1
services:
  y:
    command: ["sh", "-c", "echo y-new; sleep 60"]
    disabled: true
    logs:
      retained_runs: 2
  z:
    command: ["sh", "-c", "echo z-new; sleep 60"]
"#,
    )?;
    let dry_run = assert_reconcile_dry_run(&harness, &before_x, &before_y).await?;
    apply_reconcile_and_assert(&harness, &config_path, &before_x, &before_y, &dry_run).await?;

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn reconcile_rejects_dynamic_dependencies_collisions_and_invalid_files_without_mutation()
-> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("micromux.yaml");
    fs::write(&config_path, reload_test_yaml("base-running"))?;
    let services = services_from_config_path(&config_path)?;
    let harness = spawn_harness_with_policy(
        services,
        Some(ReloadConfig {
            config_path: config_path.clone(),
            strict_override: None,
        }),
        dir.path().to_path_buf(),
        enabled_dynamic_policy(dir.path())?,
    );
    wait_for_log(&harness.reader, "svc", "base-running").await?;
    let mut params = dynamic_params("job", &["sh", "-c", "sleep 60"]);
    params.spec.depends_on = Some(vec![crate::DependencySpec {
        service: "svc".to_string(),
        condition: config::DependencyCondition::Started,
    }]);
    dynamic_accepted(harness.control.start_dynamic(params).await)?;
    wait_until(&harness.reader, "job", |snapshot| {
        snapshot.execution == Execution::Running
    })
    .await?;

    fs::write(&config_path, "version: 1\nservices: {}\n")?;
    let depended_on = harness
        .control
        .reconcile_config(false)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(
        matches!(depended_on, Err(CommandRejection::InvalidSpec(message)) if message.contains("job"))
    );
    assert!(harness.reader.service("svc").is_some());

    fs::write(
        &config_path,
        r#"version: 1
services:
  svc:
    command: ["sh", "-c", "echo base-running; sleep 60"]
  job:
    command: ["true"]
"#,
    )?;
    let collision = harness
        .control
        .reconcile_config(false)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(
        matches!(collision, Err(CommandRejection::InvalidSpec(message)) if message.contains("live or retired dynamic service"))
    );
    assert_eq!(harness.reader.services().len(), 2);
    dynamic_accepted(harness.control.stop_dynamic(&"job".to_string()).await)?;
    wait_until(&harness.reader, "job", |snapshot| {
        snapshot.retired == Some(RetiredReason::Stopped)
    })
    .await?;
    let retired_collision = harness
        .control
        .reconcile_config(false)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(
        matches!(retired_collision, Err(CommandRejection::InvalidSpec(message)) if message.contains("live or retired dynamic service"))
    );

    let before = harness
        .reader
        .service("svc")
        .ok_or_else(|| eyre::eyre!("missing svc"))?;
    fs::write(
        &config_path,
        r"version: 1
services:
  svc:
    working_dir: ./
",
    )?;
    let invalid = harness
        .control
        .reconcile_config(false)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(matches!(invalid, Err(CommandRejection::ConfigReload(_))));
    let after = harness
        .reader
        .service("svc")
        .ok_or_else(|| eyre::eyre!("svc disappeared"))?;
    assert_eq!(after.run_generation, before.run_generation);
    assert_eq!(after.pid, before.pid);
    assert_eq!(harness.reader.services().len(), 2);

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn reconcile_requires_a_reloadable_config_path() -> eyre::Result<()> {
    let harness = spawn_harness(ServiceMap::new(), None);
    let result = harness
        .control
        .reconcile_config(false)
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(
        matches!(result, Err(CommandRejection::ConfigReload(message)) if message.contains("no config path"))
    );
    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn reconcile_add_honors_disabled_startup_mode() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("micromux.yaml");
    fs::write(&config_path, "version: 1\nservices: {}\n")?;
    let services = services_from_config_path(&config_path)?;
    let harness = spawn_harness(
        services,
        Some(ReloadConfig {
            config_path: config_path.clone(),
            strict_override: None,
        }),
    );
    fs::write(
        &config_path,
        r#"version: 1
services:
  parked:
    command: ["sh", "-c", "sleep 60"]
    disabled: true
"#,
    )?;

    reconcile_accepted(harness.control.reconcile_config(false).await)?;
    let parked = harness
        .reader
        .service("parked")
        .ok_or_else(|| eyre::eyre!("disabled addition is missing"))?;
    assert_eq!(parked.desired, Desired::Disabled);
    assert_eq!(parked.execution, Execution::Pending);
    assert_eq!(parked.run_generation, 0);
    assert_eq!(parked.pid, None);

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn restart_reloads_latest_service_config_before_spawning() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("micromux.yaml");
    fs::write(&config_path, reload_test_yaml("old-config"))?;
    let services = services_from_config_path(&config_path)?;
    let harness = spawn_harness(
        services,
        Some(ReloadConfig {
            config_path: config_path.clone(),
            strict_override: None,
        }),
    );
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
async fn reload_preserves_dynamic_roster_and_rejects_collisions() -> eyre::Result<()> {
    let dir = tempfile::tempdir()?;
    let config_path = dir.path().join("micromux.yaml");
    fs::write(&config_path, reload_test_yaml("old-config"))?;
    let services = services_from_config_path(&config_path)?;
    let harness = spawn_harness_with_policy(
        services,
        Some(ReloadConfig {
            config_path: config_path.clone(),
            strict_override: None,
        }),
        dir.path().to_path_buf(),
        enabled_dynamic_policy(dir.path())?,
    );
    wait_for_log(&harness.reader, "svc", "old-config").await?;
    dynamic_accepted(
        harness
            .control
            .start_dynamic(dynamic_params("debug", &["sh", "-c", "sleep 60"]))
            .await,
    )?;
    wait_until(&harness.reader, "debug", |snapshot| {
        snapshot.execution == Execution::Running
    })
    .await?;

    fs::write(&config_path, reload_test_yaml("new-config"))?;
    accepted(harness.control.restart(&"svc".to_string()).await)?;
    wait_for_log(&harness.reader, "svc", "new-config").await?;
    assert!(
        harness
            .reader
            .service("debug")
            .is_some_and(|snapshot| snapshot.origin == crate::OriginKind::Dynamic)
    );

    fs::write(
        &config_path,
        r#"version: 1
services:
  svc:
    command: ["sh", "-c", "sleep 60"]
  debug:
    command: ["true"]
"#,
    )?;
    let collision = harness
        .control
        .restart(&"svc".to_string())
        .await
        .map_err(|_| eyre::eyre!("scheduler stopped"))?;
    assert!(
        matches!(collision, Err(CommandRejection::ConfigReload(message)) if message.contains("exists as a dynamic service"))
    );

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
    let harness = spawn_harness(
        services,
        Some(ReloadConfig {
            config_path: config_path.clone(),
            strict_override: None,
        }),
    );
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
    let harness = spawn_harness(
        services,
        Some(ReloadConfig {
            config_path: config_path.clone(),
            strict_override: None,
        }),
    );

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
    let harness = spawn_harness(
        services,
        Some(ReloadConfig {
            config_path: config_path.clone(),
            strict_override: None,
        }),
    );
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
    let harness = spawn_harness(services, None);

    let attempt = wait_for_finished_health_attempt(&harness.reader, "svc").await?;
    assert_eq!(attempt.result.map(|result| result.success), Some(true));

    harness.shutdown.cancel();
    harness.handle.await??;
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
    let harness = spawn_harness(services, None);

    let attempt = wait_for_finished_health_attempt(&harness.reader, "svc").await?;
    assert_eq!(attempt.result.map(|result| result.success), Some(true));

    harness.shutdown.cancel();
    harness.handle.await??;
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
    let harness = spawn_harness(services, None);

    let attempt = wait_for_finished_health_attempt(&harness.reader, "svc").await?;
    assert!(
        attempt
            .output
            .iter()
            .any(|line| { line.stream == OutputStream::Stderr && !line.line.is_empty() })
    );
    assert!(
        attempt.result.is_some_and(|result| {
            !result.success && result.exit_code == -1 && !result.cancelled
        })
    );

    harness.shutdown.cancel();
    harness.handle.await??;
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
async fn restart_timeline_orders_request_spawn_and_exit() -> eyre::Result<()> {
    let mut services = ServiceMap::new();
    services.insert(
        "svc".to_string(),
        Service::new("svc", Path::new("."), service_config("svc", ("true", &[])))?,
    );
    let harness = spawn_harness(services, None);
    wait_until(&harness.reader, "svc", |snapshot| {
        snapshot.execution == Execution::Exited
    })
    .await?;
    accepted(harness.control.restart(&"svc".to_string()).await)?;
    wait_until(&harness.reader, "svc", |snapshot| {
        snapshot.run_generation >= 2 && snapshot.execution == Execution::Exited
    })
    .await?;

    let kinds = harness
        .reader
        .events("svc", None, None)
        .0
        .into_iter()
        .map(|event| event.kind)
        .collect::<Vec<_>>();
    let request = kinds
        .iter()
        .rposition(|kind| *kind == ServiceEventKind::RestartRequested)
        .ok_or_else(|| eyre::eyre!("restart-requested event missing"))?;
    assert_eq!(
        kinds.get(request..request + 3),
        Some(
            [
                ServiceEventKind::RestartRequested,
                ServiceEventKind::Spawned,
                ServiceEventKind::Exited,
            ]
            .as_slice()
        )
    );

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn crash_timeline_records_backoff_delay() -> eyre::Result<()> {
    let mut cfg = service_config("svc", ("sh", &["-c", "exit 1"]));
    cfg.restart_policy = crate::service::RestartPolicy::Always;
    let mut services = ServiceMap::new();
    services.insert("svc".to_string(), Service::new("svc", Path::new("."), cfg)?);
    let harness = spawn_harness(services, None);
    wait_until(&harness.reader, "svc", |snapshot| {
        snapshot.run_generation >= 2
    })
    .await?;

    assert!(
        harness
            .reader
            .events("svc", None, None)
            .0
            .iter()
            .any(|event| {
                event.kind == ServiceEventKind::BackoffScheduled
                    && event.delay_ms.is_some_and(|delay| delay > 0)
            })
    );

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}

#[tokio::test]
async fn auto_restarts_failing_service_without_manual_command() -> eyre::Result<()> {
    let config_dir = Path::new(".");
    let mut cfg = service_config("svc", ("sh", &["-c", "exit 1"]));
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
    cfg.restart_policy = crate::service::RestartPolicy::Always;

    let mut services: ServiceMap = ServiceMap::new();
    services.insert("svc".to_string(), Service::new("svc", &dir, cfg)?);
    let harness = spawn_harness(services, None);

    wait_until(&harness.reader, "svc", |snapshot| {
        snapshot.run_generation >= 2 && snapshot.execution == Execution::Running
    })
    .await?;
    wait_for_log(&harness.reader, "svc", "second-run").await?;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        !harness
            .reader
            .logs("svc", None)
            .iter()
            .any(|line| { line.line.contains("stale-from-first-run") })
    );

    harness.shutdown.cancel();
    harness.handle.await??;
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
    let harness = spawn_harness(services, None);

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
    let harness = spawn_harness(services, None);

    wait_for_log(&harness.reader, "svc", "first-run").await?;
    accepted(harness.control.disable(&"svc".to_string()).await)?;
    wait_until(&harness.reader, "svc", |snapshot| {
        snapshot.desired == Desired::Disabled && snapshot.execution == Execution::Exited
    })
    .await?;

    accepted(harness.control.enable(&"svc".to_string()).await)?;
    wait_until(&harness.reader, "svc", |snapshot| {
        snapshot.run_generation == 2 && snapshot.execution == Execution::Running
    })
    .await?;
    assert!(
        harness
            .reader
            .logs("svc", None)
            .iter()
            .any(|line| { line.line.contains("first-run") })
    );

    harness.shutdown.cancel();
    harness.handle.await??;
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
    let harness = spawn_harness(services, None);

    wait_for_log(&harness.reader, "svc", "hello").await?;
    wait_for_log(&harness.reader, "svc", "err").await?;

    harness.shutdown.cancel();
    harness.handle.await??;
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
    let harness = spawn_harness(services, None);

    wait_for_log(&harness.reader, "svc", "connecting to qdrant").await?;
    let line = harness
        .reader
        .logs("svc", None)
        .into_iter()
        .find(|line| line.line.contains("connecting to qdrant"))
        .ok_or_else(|| eyre::eyre!("missing structured log record"))?;
    assert_eq!(line.line, json);
    assert_eq!(line.line.find('\n'), None);

    harness.shutdown.cancel();
    harness.handle.await??;
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
    let harness = spawn_harness(services, None);

    wait_for_log(&harness.reader, "svc", "tty").await?;
    assert!(
        harness
            .reader
            .logs("svc", None)
            .iter()
            .any(|line| { line.line == "tty" })
    );

    harness.shutdown.cancel();
    harness.handle.await??;
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
    let harness = spawn_harness(services, None);

    wait_until(&harness.reader, "svc", |snapshot| {
        snapshot.execution == Execution::Running
    })
    .await?;
    harness
        .commands
        .send(Command::SendInput("svc".to_string(), b"hello\r".to_vec()))
        .await?;
    wait_for_log(&harness.reader, "svc", "got:hello").await?;

    harness.shutdown.cancel();
    harness.handle.await??;
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
    let harness = spawn_harness(services, None);

    harness
        .commands
        .send(Command::ResizeAll {
            cols: 100,
            rows: 40,
        })
        .await?;
    wait_for_log(&harness.reader, "app", "40 100").await?;
    harness
        .commands
        .send(Command::SendInput("app".to_string(), b"go\r".to_vec()))
        .await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let count = harness
            .reader
            .logs("app", None)
            .iter()
            .filter(|line| line.line.trim() == "40 100")
            .count();
        if count >= 2 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            eyre::bail!("app did not report the resized PTY twice");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    harness.shutdown.cancel();
    harness.handle.await??;
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
    let harness = spawn_harness(services, None);
    let expected = work_abs.canonicalize()?;
    wait_for_log(&harness.reader, "svc", expected.to_string_lossy().as_ref()).await?;
    assert!(
        harness
            .reader
            .logs("svc", None)
            .iter()
            .any(|line| { Path::new(&line.line) == expected })
    );

    harness.shutdown.cancel();
    harness.handle.await??;
    Ok(())
}
