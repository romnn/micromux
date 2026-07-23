//! End-to-end tests for the remote TUI mirror.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use color_eyre::eyre;
use micromux::CancellationToken;
use micromux_control::{ControlEndpoint, ControlServer, SessionIdentity, bind};
use micromux_tui::RemoteSource;

struct Session {
    endpoint: ControlEndpoint,
    shutdown: CancellationToken,
    _runner: tokio::task::JoinHandle<eyre::Result<()>>,
}

fn build_session(dir: &Path) -> eyre::Result<Session> {
    let yaml = r#"version: 1
services:
  svc:
    command: ["sh", "-c", "echo attached-line; sleep 60"]
"#;
    let mut diagnostics = vec![];
    let mut config = micromux::from_str(yaml, dir, 0usize, None, &mut diagnostics)
        .map_err(|err| eyre::eyre!("parse config: {err}"))?;
    let config_path = dir.join("micromux.yaml");
    std::fs::write(&config_path, yaml)?;
    config.config_path = Some(config_path.clone());
    let mux = Arc::new(micromux::Micromux::new(&config)?);

    let shutdown = CancellationToken::new();
    let (runner, handles) = mux.start(shutdown.clone());
    let runner = tokio::spawn(runner);
    let endpoint = micromux_control::endpoint_for(dir, &config_path);
    let guard = bind(&endpoint)?.ok_or_else(|| eyre::eyre!("failed to bind endpoint"))?;
    let identity = SessionIdentity::new("attach-test".to_string(), dir, &config_path);
    let service_control = handles.service_control();
    let server = Arc::new(ControlServer::new(
        handles.reader,
        service_control,
        identity,
        handles.dynamic_services,
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

#[tokio::test]
async fn remote_source_observes_logs_and_restart_generation() -> eyre::Result<()> {
    let dir = tempfile::Builder::new()
        .prefix("micromux-tui-remote-")
        .tempdir()?;
    let session = build_session(dir.path())?;
    let source = RemoteSource::connect(session.endpoint.clone()).await?;

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if source
            .logs_since("svc", 0)
            .1
            .iter()
            .any(|line| line.line.contains("attached-line"))
        {
            break;
        }
        if Instant::now() >= deadline {
            eyre::bail!("remote source did not observe the service log");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let initial_generation = source
        .service("svc")
        .ok_or_else(|| eyre::eyre!("remote source omitted svc"))?
        .run_generation;
    source.restart("svc".to_string());

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if source
            .service("svc")
            .is_some_and(|snapshot| snapshot.run_generation > initial_generation)
        {
            break;
        }
        if Instant::now() >= deadline {
            eyre::bail!("remote source did not observe the restart generation");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    source.cancel();
    session.shutdown.cancel();
    Ok(())
}
