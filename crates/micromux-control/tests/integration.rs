//! End-to-end control-plane tests: boot a real core, bind an endpoint, drive it over the socket.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use color_eyre::eyre;
use micromux::CancellationToken;
use micromux_control::{
    Client, ControlEndpoint, ControlServer, EndpointProbeResult, Request, Response,
    SessionIdentity, bind, endpoint_owner_lock_held, probe_runtime_dir,
};
use similar_asserts::assert_eq;

fn unique_dir(prefix: &str) -> eyre::Result<tempfile::TempDir> {
    Ok(tempfile::Builder::new()
        .prefix(&format!("micromux-control-{prefix}-"))
        .tempdir()?)
}

struct Session {
    endpoint: ControlEndpoint,
    shutdown: CancellationToken,
    _runner: tokio::task::JoinHandle<eyre::Result<()>>,
}

fn build_session(dir: &Path, command: &str) -> eyre::Result<Session> {
    let yaml =
        format!("version: 1\nservices:\n  svc:\n    command: [\"sh\", \"-c\", \"{command}\"]\n");
    build_session_yaml(dir, &yaml)
}

fn build_session_yaml(dir: &Path, yaml: &str) -> eyre::Result<Session> {
    let mut diagnostics = vec![];
    let mut config = micromux::from_str(yaml, dir, 0usize, None, &mut diagnostics)
        .map_err(|err| eyre::eyre!("parse config: {err}"))?;
    let config_path = dir.join("micromux.yaml");
    std::fs::write(&config_path, yaml)?;
    config.config_path = Some(config_path.clone());
    let mux = Arc::new(micromux::Micromux::new(&config)?);

    let shutdown = CancellationToken::new();
    let (runner, handles) = mux.clone().start(shutdown.clone());
    let runner = tokio::spawn(runner);

    let endpoint = micromux_control::endpoint_for(dir, &config_path);
    let guard = bind(&endpoint)?.ok_or_else(|| eyre::eyre!("failed to acquire endpoint lock"))?;
    let identity = SessionIdentity::new("test".to_string(), dir, &config_path);
    let server = Arc::new(ControlServer::new(
        handles.reader.clone(),
        handles.service_control(),
        identity,
        handles.dynamic_services.clone(),
    ));
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move { server.serve(guard, shutdown).await }
    });

    Ok(Session {
        endpoint,
        shutdown,
        _runner: runner,
    })
}

fn dynamic_params(id: &str, command: &[&str]) -> micromux::DynamicServiceParams {
    let spec = micromux::PartialServiceSpec {
        command: Some(command.iter().map(|part| (*part).to_string()).collect()),
        environment: Some(
            [("PRIVATE_VALUE".to_string(), "secret".to_string())]
                .into_iter()
                .collect(),
        ),
        ..micromux::PartialServiceSpec::default()
    };
    micromux::DynamicServiceParams {
        service: id.to_string(),
        spec,
        from_service: None,
        extra_args: Vec::new(),
        expires_after: Some(micromux::Lease::After(Duration::from_secs(30))),
        owner: Some("integration-test".to_string()),
        idempotency_key: None,
    }
}

async fn dynamic_caps(client: &mut Client) -> eyre::Result<micromux_control::DynamicServicesCaps> {
    client
        .describe()
        .await?
        .capabilities
        .and_then(|capabilities| capabilities.dynamic_services)
        .ok_or_else(|| eyre::eyre!("dynamic capability missing"))
}

async fn renew_dynamic_over_socket(client: &mut Client) -> eyre::Result<()> {
    let renewed = client
        .request(Request::RenewDynamicService {
            service: "debug".to_string(),
            expected_revision: 1,
            expires_after: Some(micromux::Lease::After(Duration::from_mins(30))),
        })
        .await?;
    let Response::DynamicService(renewed) = renewed else {
        eyre::bail!("expected dynamic-service receipt, got {renewed:?}");
    };
    assert_eq!(renewed.revision, 1);
    assert!(renewed.expires_at_unix_ms.is_some());
    Ok(())
}

async fn request_until<F>(
    endpoint: &ControlEndpoint,
    request: Request,
    mut predicate: F,
) -> eyre::Result<Response>
where
    F: FnMut(&Response) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(mut client) = Client::connect(endpoint).await {
            let response = client.request(request.clone()).await?;
            if predicate(&response) {
                return Ok(response);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            eyre::bail!("timed out waiting for a matching response");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn describe_list_logs_and_restart_over_the_socket() -> eyre::Result<()> {
    let dir = unique_dir("e2e")?;
    let session = build_session(dir.path(), "echo hello-from-svc; sleep 60")?;

    // Describe carries identity + the configured service.
    let mut client = Client::connect(&session.endpoint).await?;
    let info = client.describe().await?;
    assert_eq!(info.name, "test");
    assert_eq!(
        info.id,
        micromux_control::endpoint_hash(&dir.path().join("micromux.yaml"))
    );
    assert_eq!(info.services.len(), 1);
    assert_eq!(
        info.services.first().map(|s| s.id.clone()),
        Some("svc".to_string())
    );

    // The service eventually reports Running.
    let services = request_until(&session.endpoint, Request::ListServices, |response| {
        matches!(response, Response::Services(services)
            if services.first().is_some_and(|s| matches!(s.execution, micromux::Execution::Running)))
    })
    .await?;
    assert!(matches!(services, Response::Services(_)));

    // Logs flow through the model to the socket.
    request_until(
        &session.endpoint,
        Request::GetLogs {
            service: "svc".to_string(),
            run_generation: None,
            tail: None,
        },
        |response| {
            matches!(response, Response::Logs { lines, .. }
                if lines.iter().any(|line| line.line.contains("hello-from-svc")))
        },
    )
    .await?;

    // Restart is Accepted and carries the pre-restart generation.
    let restart = client
        .request(Request::Restart {
            service: "svc".to_string(),
        })
        .await?;
    match restart {
        Response::Accepted { services } => {
            assert_eq!(services.first().map(|a| a.observed_generation), Some(1));
        }
        other => eyre::bail!("expected Accepted, got {other:?}"),
    }

    // An unknown service is a typed error, not a panic.
    let unknown = client
        .request(Request::Restart {
            service: "nope".to_string(),
        })
        .await?;
    assert!(matches!(
        unknown,
        Response::Error {
            code: micromux_control::ErrorCode::UnknownService,
            ..
        }
    ));

    let unknown_logs = client
        .request(Request::GetLogs {
            service: "nope".to_string(),
            run_generation: None,
            tail: None,
        })
        .await?;
    assert!(matches!(
        unknown_logs,
        Response::Error {
            code: micromux_control::ErrorCode::UnknownService,
            ..
        }
    ));

    let unknown_health = client
        .request(Request::GetHealth {
            service: "nope".to_string(),
        })
        .await?;
    assert!(matches!(
        unknown_health,
        Response::Error {
            code: micromux_control::ErrorCode::UnknownService,
            ..
        }
    ));

    session.shutdown.cancel();
    Ok(())
}

#[tokio::test]
async fn reconcile_dry_run_and_apply_cross_the_socket() -> eyre::Result<()> {
    let dir = unique_dir("reconcile")?;
    let session = build_session(dir.path(), "sleep 60")?;
    let config_path = dir.path().join("micromux.yaml");
    std::fs::write(
        &config_path,
        r#"version: 1
services:
  svc:
    command: ["sh", "-c", "sleep 60"]
  added:
    command: ["sh", "-c", "sleep 60"]
"#,
    )?;
    let mut client = Client::connect(&session.endpoint).await?;

    let dry_run = client
        .request(Request::ReconcileConfig { dry_run: true })
        .await?;
    let Response::Reconcile(dry_run) = dry_run else {
        eyre::bail!("expected reconcile receipt, got {dry_run:?}");
    };
    assert!(dry_run.dry_run);
    assert_eq!(dry_run.actions.len(), 1);
    assert_eq!(dry_run.actions[0].service, "added");
    assert_eq!(
        dry_run.actions[0].action,
        micromux::ReconcileActionKind::Added
    );
    let before_apply = client.request(Request::ListServices).await?;
    assert!(matches!(before_apply, Response::Services(services) if services.len() == 1));

    let applied = client
        .request(Request::ReconcileConfig { dry_run: false })
        .await?;
    let Response::Reconcile(applied) = applied else {
        eyre::bail!("expected reconcile receipt, got {applied:?}");
    };
    assert!(!applied.dry_run);
    assert_eq!(applied.actions, dry_run.actions);
    request_until(&session.endpoint, Request::ListServices, |response| {
        matches!(response, Response::Services(services)
        if services.iter().any(|service| {
            service.id == "added" && service.execution == micromux::Execution::Running
        }))
    })
    .await?;

    session.shutdown.cancel();
    Ok(())
}

#[tokio::test]
async fn dynamic_service_lifecycle_and_capabilities_cross_the_socket() -> eyre::Result<()> {
    let dir = unique_dir("dynamic")?;
    let session = build_session_yaml(
        dir.path(),
        r#"version: 1
control:
  dynamic_services:
    enabled: true
    allowed_working_roots: [.]
    max_services: 2
    max_lifetime: 1h
services:
  svc:
    command: ["sh", "-c", "sleep 60"]
    environment:
      CLONED_SECRET: hidden
"#,
    )?;
    let mut client = Client::connect(&session.endpoint).await?;
    let before_caps = dynamic_caps(&mut client).await?;
    assert_eq!(before_caps.max_services, 2);
    assert_eq!(before_caps.live_services, 0);

    let mut params = dynamic_params("debug", &["sh", "-c", "echo debug; sleep 60"]);
    params.from_service = Some("svc".to_string());
    params.extra_args.push("clone-arg".to_string());
    let created = client
        .request(Request::StartDynamicService { params })
        .await?;
    let Response::DynamicService(created) = created else {
        eyre::bail!("expected dynamic-service receipt, got {created:?}");
    };
    assert_eq!(created.revision, 1);
    assert_eq!(created.env_keys, vec!["CLONED_SECRET", "PRIVATE_VALUE"]);
    assert_eq!(
        created.command.last().map(String::as_str),
        Some("clone-arg")
    );
    assert!(!serde_json::to_string(&created)?.contains("secret"));

    request_until(&session.endpoint, Request::ListServices, |response| {
        matches!(response, Response::Services(services)
        if services.iter().any(|service| {
            service.id == "debug"
                && service.origin == micromux::OriginKind::Dynamic
                && service.execution == micromux::Execution::Running
        }))
    })
    .await?;
    assert_eq!(dynamic_caps(&mut client).await?.live_services, 1);

    renew_dynamic_over_socket(&mut client).await?;

    let stale = client
        .request(Request::ReplaceDynamicService {
            service: "debug".to_string(),
            expected_revision: 0,
            params: dynamic_params("debug", &["true"]),
        })
        .await?;
    assert!(matches!(
        stale,
        Response::Error {
            code: micromux_control::ErrorCode::RevisionMismatch,
            ..
        }
    ));

    let replaced = client
        .request(Request::ReplaceDynamicService {
            service: "debug".to_string(),
            expected_revision: 1,
            params: dynamic_params("debug", &["sh", "-c", "echo replaced; sleep 60"]),
        })
        .await?;
    let Response::DynamicService(replaced) = replaced else {
        eyre::bail!("expected dynamic-service receipt, got {replaced:?}");
    };
    assert_eq!(replaced.revision, 2);
    assert!(!replaced.idempotent_replay);

    let stopped = client
        .request(Request::StopDynamicService {
            service: "debug".to_string(),
        })
        .await?;
    assert!(matches!(stopped, Response::DynamicService(receipt) if !receipt.already_retired));
    request_until(&session.endpoint, Request::ListServices, |response| {
        matches!(response, Response::Services(services)
        if services.iter().any(|service| {
            service.id == "debug"
                && service.retired == Some(micromux::RetiredReason::Stopped)
        }))
    })
    .await?;
    assert_eq!(dynamic_caps(&mut client).await?.live_services, 0);

    session.shutdown.cancel();
    Ok(())
}

#[tokio::test]
async fn service_events_page_and_bound_over_the_socket() -> eyre::Result<()> {
    let dir = unique_dir("events")?;
    let session = build_session_yaml(
        dir.path(),
        r#"version: 1
control:
  dynamic_services:
    enabled: true
services:
  svc:
    command: ["sh", "-c", "sleep 60"]
"#,
    )?;
    let mut client = Client::connect(&session.endpoint).await?;
    let created = client
        .request(Request::StartDynamicService {
            params: dynamic_params("debug", &["sh", "-c", "sleep 60"]),
        })
        .await?;
    eyre::ensure!(
        matches!(created, Response::DynamicService(_)),
        "expected dynamic-service receipt, got {created:?}"
    );

    // Created lands with the insert; Spawned once the scheduler starts the run.
    let full = request_until(
        &session.endpoint,
        Request::GetEvents {
            service: "debug".to_string(),
            after: None,
            tail: Some(micromux::EVENT_HISTORY + 1),
        },
        |response| matches!(response, Response::Events { events, .. } if events.len() >= 2),
    )
    .await?;
    let Response::Events {
        events,
        truncated: false,
    } = full
    else {
        eyre::bail!("expected an untruncated event list, got {full:?}");
    };
    assert!(
        events
            .iter()
            .any(|event| event.kind == micromux::ServiceEventKind::Created)
    );
    let first_seq = events
        .first()
        .map(|event| event.seq)
        .ok_or_else(|| eyre::eyre!("event list was empty"))?;

    // An explicit tail below the event count is clamped and reported as truncated.
    let bounded = client
        .request(Request::GetEvents {
            service: "debug".to_string(),
            after: None,
            tail: Some(1),
        })
        .await?;
    assert!(matches!(bounded, Response::Events { events, truncated: true } if events.len() == 1));

    // The `after` cursor pages strictly past the given seq.
    let paged = client
        .request(Request::GetEvents {
            service: "debug".to_string(),
            after: Some(first_seq),
            tail: None,
        })
        .await?;
    let Response::Events { events: later, .. } = paged else {
        eyre::bail!("expected an event list, got {paged:?}");
    };
    assert!(!later.is_empty());
    assert!(later.iter().all(|event| event.seq > first_seq));

    session.shutdown.cancel();
    Ok(())
}

#[tokio::test]
async fn retained_run_logs_are_queryable_after_restart() -> eyre::Result<()> {
    let dir = unique_dir("run-logs")?;
    let counter = dir.path().join("counter");
    let command = format!(
        "n=$(cat {} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {}; echo run-$n; sleep 60",
        counter.display(),
        counter.display()
    );
    let session = build_session(dir.path(), &command)?;

    request_until(
        &session.endpoint,
        Request::GetLogs {
            service: "svc".to_string(),
            run_generation: None,
            tail: None,
        },
        |response| {
            matches!(response, Response::Logs { lines, .. }
                if lines.iter().any(|line| line.line.contains("run-1")))
        },
    )
    .await?;

    let mut client = Client::connect(&session.endpoint).await?;
    let restart = client
        .request(Request::Restart {
            service: "svc".to_string(),
        })
        .await?;
    assert!(matches!(restart, Response::Accepted { .. }));

    request_until(&session.endpoint, Request::ListServices, |response| {
        matches!(response, Response::Services(services)
            if services.first().is_some_and(|svc| svc.run_generation == 2))
    })
    .await?;

    let runs = client
        .request(Request::ListLogRuns {
            service: "svc".to_string(),
        })
        .await?;
    match runs {
        Response::LogRuns { runs } => {
            let generations: Vec<u64> = runs.into_iter().map(|run| run.run_generation).collect();
            assert_eq!(generations, vec![1, 2]);
        }
        other => eyre::bail!("expected LogRuns, got {other:?}"),
    }

    let previous = client
        .request(Request::GetLogs {
            service: "svc".to_string(),
            run_generation: Some(1),
            tail: None,
        })
        .await?;
    assert!(matches!(previous, Response::Logs { lines, .. }
        if lines.iter().any(|line| line.line.contains("run-1"))));

    session.shutdown.cancel();
    Ok(())
}

#[tokio::test]
async fn concurrent_same_config_startup_binds_exactly_once() -> eyre::Result<()> {
    let dir = unique_dir("concurrent")?;
    let endpoint = ControlEndpoint::Unix(dir.path().join("a.sock"));

    let first = bind(&endpoint)?;
    assert!(first.is_some(), "first bind acquires the lock");
    // A second instance on the same config cannot acquire the lock and must not create a socket.
    let second = bind(&endpoint)?;
    assert!(second.is_none(), "second bind runs with control disabled");

    // Releasing the first (process exit) lets a successor reclaim.
    drop(first);
    let third = bind(&endpoint)?;
    assert!(third.is_some(), "successor reclaims after release");

    Ok(())
}

#[tokio::test]
async fn leaked_socket_is_reclaimed() -> eyre::Result<()> {
    let dir = unique_dir("leaked")?;
    let socket_path = dir.path().join("b.sock");
    let endpoint = ControlEndpoint::Unix(socket_path.clone());

    // Simulate a crash-leaked socket: the file exists but no process holds the lock.
    std::fs::write(&socket_path, b"")?;
    assert!(socket_path.exists());

    let guard = bind(&endpoint)?;
    assert!(
        guard.is_some(),
        "a leaked socket with no live owner is reclaimed"
    );

    Ok(())
}

#[tokio::test]
async fn endpoint_owner_lock_probe_is_read_only() -> eyre::Result<()> {
    let dir = unique_dir("owner-lock")?;
    let endpoint = ControlEndpoint::Unix(dir.path().join("owned.sock"));

    assert!(!endpoint_owner_lock_held(&endpoint)?);

    let guard = bind(&endpoint)?.ok_or_else(|| eyre::eyre!("bind failed"))?;
    assert!(endpoint_owner_lock_held(&endpoint)?);

    drop(guard);
    assert!(!endpoint_owner_lock_held(&endpoint)?);
    Ok(())
}

#[tokio::test]
async fn subscribe_streams_liveness_changes() -> eyre::Result<()> {
    let dir = unique_dir("subscribe")?;
    let session = build_session(dir.path(), "sleep 60")?;

    request_until(&session.endpoint, Request::ListServices, |response| {
        matches!(response, Response::Services(services)
            if services.first().is_some_and(|s| matches!(s.execution, micromux::Execution::Running)))
    })
    .await?;

    let mut subscription = Client::subscribe(&session.endpoint).await?;
    let mut client = Client::connect(&session.endpoint).await?;
    client
        .request(Request::Restart {
            service: "svc".to_string(),
        })
        .await?;

    let change = tokio::time::timeout(Duration::from_secs(2), subscription.recv())
        .await??
        .ok_or_else(|| eyre::eyre!("subscription closed"))?;
    assert_eq!(change.service_id, "svc".to_string());

    session.shutdown.cancel();
    Ok(())
}

#[tokio::test]
async fn one_unreachable_endpoint_does_not_de_list_healthy_sessions() -> eyre::Result<()> {
    use tokio::io::AsyncWriteExt;

    let dir = unique_dir("discover")?;
    let session = build_session(dir.path(), "sleep 60")?;

    // A live socket that answers with garbage fails `Describe` fast with a *non-hard* error. It must
    // be counted as unreachable — never abort the scan or hide the healthy session.
    let garbage_path = dir.path().join("garbage.sock");
    let listener = tokio::net::UnixListener::bind(&garbage_path)?;
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = stream.write_all(b"not json\n").await;
            let _ = stream.flush().await;
        }
    });

    let closed_path = dir.path().join("closed.sock");
    let listener = tokio::net::UnixListener::bind(&closed_path)?;
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });

    let probes = probe_runtime_dir(dir.path()).await?;
    let sessions = probes
        .iter()
        .filter(|probe| matches!(&probe.result, EndpointProbeResult::Session(_)))
        .count();
    let unreachable = probes
        .iter()
        .filter(|probe| matches!(&probe.result, EndpointProbeResult::Unreachable(_)))
        .count();
    assert_eq!(sessions, 1, "the healthy session must still be listed");
    assert_eq!(
        unreachable, 2,
        "live-but-bad endpoints must be counted, not hidden"
    );

    session.shutdown.cancel();
    Ok(())
}

#[tokio::test]
async fn shutdown_request_stops_the_session_and_removes_the_endpoint() -> eyre::Result<()> {
    let dir = unique_dir("shutdown")?;
    let session = build_session(dir.path(), "sleep 300")?;

    // It is serving before the request.
    request_until(&session.endpoint, Request::ListServices, |response| {
        matches!(response, Response::Services(_))
    })
    .await?;

    // Shutdown is acknowledged...
    let mut client = Client::connect(&session.endpoint).await?;
    let response = client.request(Request::Shutdown).await?;
    assert!(matches!(response, Response::ShuttingDown));

    // ...the shared session token is cancelled (so the scheduler/TUI also wind down)...
    tokio::time::timeout(Duration::from_secs(2), session.shutdown.cancelled())
        .await
        .map_err(|_| eyre::eyre!("shutdown token was not cancelled"))?;

    // ...and the endpoint stops answering (the guard unlinked the socket on the way out).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if Client::connect(&session.endpoint).await.is_err() {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            eyre::bail!("endpoint still answering after shutdown");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

// Note: the forward-paging fix for a retained run longer than MAX_LOG_TAIL is tested
// deterministically at the model level — `model::tests::run_log_after_pages_from_the_beginning_*` —
// because the PTY/terminal capture samples a fast burst (it does not emit one record per line for
// rapid output), so a service cannot reliably produce >2000 distinct records over the socket.
