//! End-to-end tests for the remote TUI mirror.

#![cfg(unix)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use color_eyre::eyre;
use micromux::CancellationToken;
use micromux_control::{ControlEndpoint, ControlServer, SessionIdentity, bind};
use micromux_tui::RemoteSource;
use similar_asserts::assert_eq;

struct Session {
    endpoint: ControlEndpoint,
    config_path: std::path::PathBuf,
    reader: micromux::SessionModelReader,
    shutdown: CancellationToken,
    _runner: tokio::task::JoinHandle<eyre::Result<()>>,
}

const SESSION_YAML: &str = r#"version: 1
services:
  svc:
    command: ["sh", "-c", "echo attached-line-$$; sleep 60"]
"#;

/// A service that keeps emitting so its seq space grows well past a fresh instance's first seqs.
const TICKER_YAML: &str = r#"version: 1
services:
  svc:
    command: ["sh", "-c", "echo attached-line-$$; while true; do echo tick; sleep 0.1; done"]
"#;

fn build_session(dir: &Path, yaml: &str) -> eyre::Result<Session> {
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
    let reader = handles.reader.clone();
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
        config_path,
        reader,
        shutdown,
        _runner: runner,
    })
}

async fn wait_for_running(source: &RemoteSource) -> eyre::Result<u64> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(snapshot) = source.service("svc")
            && snapshot.execution == micromux::Execution::Running
        {
            return Ok(snapshot.run_generation);
        }
        if Instant::now() >= deadline {
            eyre::bail!("remote source did not observe a running service");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn remote_source_observes_logs_and_restart_generation() -> eyre::Result<()> {
    let dir = tempfile::Builder::new()
        .prefix("micromux-tui-remote-")
        .tempdir()?;
    let session = build_session(dir.path(), SESSION_YAML)?;
    let source = RemoteSource::connect(session.endpoint.clone()).await?;

    let deadline = Instant::now() + Duration::from_secs(3);
    let initial_line = loop {
        if let Some(line) = source
            .logs_since("svc", 0)
            .1
            .into_iter()
            .find(|line| line.line.contains("attached-line"))
        {
            break line.line;
        }
        if Instant::now() >= deadline {
            eyre::bail!("remote source did not observe the service log");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    let initial_generation = wait_for_running(&source).await?;
    source.restart("svc".to_string());

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let restarted = source
            .service("svc")
            .is_some_and(|snapshot| snapshot.run_generation > initial_generation);
        let lines = source.logs_since("svc", 0).1;
        let replacement_log = lines
            .iter()
            .any(|line| line.line.contains("attached-line") && line.line != initial_line);
        if restarted && replacement_log {
            assert!(!lines.iter().any(|line| line.line == initial_line));
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

/// Follow logs exactly the way the render path does: an incremental cursor just below the newest
/// cached seq, clearing on a `None` retention marker and pruning below a `Some` one.
async fn follow_until<T>(
    source: &RemoteSource,
    cached: &mut Vec<micromux::LogLine>,
    timeout: Duration,
    check: impl Fn(&[micromux::LogLine]) -> Option<T>,
) -> eyre::Result<T> {
    let deadline = Instant::now() + timeout;
    loop {
        let after = cached.last().map_or(0, |line| line.seq.saturating_sub(1));
        let (first_retained, new_lines) = source.logs_since("svc", after);
        match first_retained {
            None => cached.clear(),
            Some(first) => cached.retain(|line| line.seq >= first),
        }
        for line in new_lines {
            match cached.last_mut() {
                Some(cached_line) if cached_line.seq == line.seq => *cached_line = line,
                _ => cached.push(line),
            }
        }
        if let Some(found) = check(cached) {
            return Ok(found);
        }
        if Instant::now() >= deadline {
            eyre::bail!("render-style log cursor did not observe the expected state");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Boot a session at the same endpoint, retrying while the previous owner's lock releases.
async fn boot_replacement(dir: &Path) -> eyre::Result<Session> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match build_session(dir, TICKER_YAML) {
            Ok(session) => return Ok(session),
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(err) => return Err(err),
        }
    }
}

#[tokio::test]
async fn replaced_session_instance_recovers_the_render_log_cursor() -> eyre::Result<()> {
    let dir = tempfile::Builder::new()
        .prefix("micromux-tui-replace-")
        .tempdir()?;
    let session = build_session(dir.path(), TICKER_YAML)?;
    let source = RemoteSource::connect(session.endpoint.clone()).await?;

    let mut cached = Vec::new();
    let initial_line = follow_until(&source, &mut cached, Duration::from_secs(3), |cached| {
        cached
            .iter()
            .find(|line| line.line.contains("attached-line"))
            .map(|line| line.line.clone())
    })
    .await?;
    // Let the first instance's seq space grow well past the seqs a fresh instance starts with, so
    // the stale cursor really points beyond everything the replacement serves.
    follow_until(&source, &mut cached, Duration::from_secs(8), |cached| {
        (cached.last().map_or(0, |line| line.seq) >= 30).then_some(())
    })
    .await?;

    session.shutdown.cancel();
    let replacement = boot_replacement(dir.path()).await?;

    // The frozen-pane regression: streaming must recover through the stale cursor without a
    // manual restart, and no zombie lines from the dead instance may survive the recovery.
    follow_until(&source, &mut cached, Duration::from_secs(10), |cached| {
        cached
            .iter()
            .any(|line| line.line.contains("attached-line") && line.line != initial_line)
            .then_some(())
    })
    .await?;
    assert!(!cached.iter().any(|line| line.line == initial_line));

    source.cancel();
    replacement.shutdown.cancel();
    Ok(())
}

#[tokio::test]
async fn detach_after_resolution_leaves_the_session_running() -> eyre::Result<()> {
    let dir = tempfile::Builder::new()
        .prefix("micromux-tui-detach-")
        .tempdir()?;
    let session = build_session(dir.path(), SESSION_YAML)?;
    let runtime_dirs = vec![dir.path().to_path_buf()];
    let dir_statuses = vec![micromux_control::RuntimeDirStatus {
        path: dir.path().to_path_buf(),
        usable: true,
        error: None,
    }];
    let resolved = micromux_control::resolve_selector_in_runtime_dirs(
        &runtime_dirs,
        &dir_statuses,
        dir.path(),
        None,
        Some(&session.config_path),
    )
    .await?;
    let source = RemoteSource::connect(resolved.endpoint).await?;
    wait_for_running(&source).await?;

    source.cancel();
    drop(source);
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(session.reader.service("svc").is_some_and(|snapshot| {
        snapshot.execution == micromux::Execution::Running && snapshot.run_generation > 0
    }));
    let mut client = micromux_control::Client::connect(&session.endpoint).await?;
    assert_eq!(client.describe().await?.name, "attach-test");

    session.shutdown.cancel();
    Ok(())
}
