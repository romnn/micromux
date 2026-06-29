//! End-to-end control-plane tests: boot a real core, bind an endpoint, drive it over the socket.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use color_eyre::eyre;
use micromux::CancellationToken;
use micromux_control::{
    Client, ControlEndpoint, ControlServer, Request, Response, SessionIdentity, bind,
};
use similar_asserts::assert_eq;

fn unique_dir(prefix: &str) -> eyre::Result<PathBuf> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let dir = std::env::temp_dir().join(format!("micromux-control-{prefix}-{nanos}"));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

struct Session {
    endpoint: ControlEndpoint,
    shutdown: CancellationToken,
    _runner: tokio::task::JoinHandle<eyre::Result<()>>,
}

fn build_session(dir: &Path, command: &str) -> eyre::Result<Session> {
    let yaml =
        format!("version: 1\nservices:\n  svc:\n    command: [\"sh\", \"-c\", \"{command}\"]\n");
    let mut diagnostics = vec![];
    let config = micromux::from_str(&yaml, Path::new("."), 0usize, None, &mut diagnostics)
        .map_err(|err| eyre::eyre!("parse config: {err}"))?;
    let mux = Arc::new(micromux::Micromux::new(&config)?);

    let shutdown = CancellationToken::new();
    let (runner, handles) = mux.clone().start(shutdown.clone());
    let runner = tokio::spawn(runner);

    let config_path = dir.join("micromux.yaml");
    let endpoint = micromux_control::endpoint_for(dir, &config_path);
    let guard = bind(&endpoint)?.ok_or_else(|| eyre::eyre!("failed to acquire endpoint lock"))?;
    let identity = SessionIdentity::new("test".to_string(), dir, &config_path);
    let server = Arc::new(ControlServer::new(
        handles.reader.clone(),
        handles.service_control(),
        identity,
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
    let session = build_session(&dir, "echo hello-from-svc; sleep 60")?;

    // Describe carries identity + the configured service.
    let mut client = Client::connect(&session.endpoint).await?;
    let info = client.describe().await?;
    assert_eq!(info.name, "test");
    assert_eq!(
        info.id,
        micromux_control::endpoint_hash(&dir.join("micromux.yaml"))
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
async fn retained_run_logs_are_queryable_after_restart() -> eyre::Result<()> {
    let dir = unique_dir("run-logs")?;
    let counter = dir.join("counter");
    let command = format!(
        "n=$(cat {} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {}; echo run-$n; sleep 60",
        counter.display(),
        counter.display()
    );
    let session = build_session(&dir, &command)?;

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
    let endpoint = ControlEndpoint::Unix(dir.join("a.sock"));

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
    let socket_path = dir.join("b.sock");
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
async fn subscribe_streams_liveness_changes() -> eyre::Result<()> {
    let dir = unique_dir("subscribe")?;
    let session = build_session(&dir, "sleep 60")?;

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
    let session = build_session(&dir, "sleep 60")?;

    // A live socket that answers with garbage fails `Describe` fast with a *non-hard* error. It must
    // be counted as unreachable — never abort the scan or hide the healthy session.
    let garbage_path = dir.join("garbage.sock");
    let listener = tokio::net::UnixListener::bind(&garbage_path)?;
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = stream.write_all(b"not json\n").await;
            let _ = stream.flush().await;
        }
    });

    let discovery = micromux_control::discover_sessions(&dir).await?;
    assert_eq!(
        discovery.sessions.len(),
        1,
        "the healthy session must still be listed"
    );
    assert_eq!(
        discovery.unreachable, 1,
        "the garbage endpoint must be counted, not hidden"
    );

    session.shutdown.cancel();
    Ok(())
}
