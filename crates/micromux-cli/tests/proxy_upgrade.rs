//! Replacing the shared proxy with a newer release, exercised with the real binary.
#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use color_eyre::eyre;
use micromux_proxy::protocol::{Registration, RouteTarget};
use micromux_proxy::{PortPreference, ProxyLauncher, ProxyPaths, Registrar, ensure_proxy};
use similar_asserts::assert_eq;

mod common;

/// A port nothing listens on right now.
fn free_port() -> eyre::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

/// Sends one proxy protocol message over `socket` and returns the reply.
fn ask_proxy(socket: &Path, message: &str) -> eyre::Result<serde_json::Value> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    writeln!(stream, "{message}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

/// The pid and startup port of the first proxy answering on `socket` for which `ready` holds on
/// its `hello` reply.
fn proxy_info(
    socket: &Path,
    ready: impl Fn(&serde_json::Value) -> bool,
) -> eyre::Result<(u64, u64)> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(info) = ask_proxy(socket, r#"{"type":"hello"}"#)
            && ready(&info)
            && let Some(pid) = info.get("pid").and_then(serde_json::Value::as_u64)
            && let Some(port) = info.get("port").and_then(serde_json::Value::as_u64)
        {
            return Ok((pid, port));
        }
        eyre::ensure!(Instant::now() < deadline, "no proxy answered as expected");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether the `hello` reply `info` lists the session `name`.
fn lists(info: &serde_json::Value, name: &str) -> bool {
    info.get("sessions")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|sessions| {
            sessions.iter().any(|session| {
                session.get("session").and_then(serde_json::Value::as_str) == Some(name)
            })
        })
}

/// A session whose launcher starts a newer release than the running proxy replaces it even while
/// another session uses it: the successor takes over the old proxy's port, the old proxy exits,
/// the other session registers again with the successor, and the new session's own port is served
/// by the successor.
// The checks below block their thread while the registrar reconnects on another.
#[tokio::test(flavor = "multi_thread")]
async fn a_newer_release_replaces_the_running_proxy() -> eyre::Result<()> {
    let project = tempfile::tempdir()?;
    let state = common::state_directory(project.path());
    let paths = ProxyPaths::in_dir(micromux_control::coordination_dir_in(&state));
    let started_port = free_port()?;
    let mut old = common::micromux(project.path())
        .args(["proxy", "--port", &started_port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let (old_pid, _) = proxy_info(&paths.socket, |_| true)?;
    assert_eq!(old_pid, u64::from(old.id()));

    // A session that uses the old proxy and cannot start one itself
    let shutdown = micromux::CancellationToken::new();
    let _registrar = Registrar::start(
        paths.clone(),
        Registration {
            git_metadata: false,
            session: "one".to_string(),
            session_domain: "one.localhost".to_string(),
            config_path: "/one/micromux.yaml".to_string(),
            working_dir: None,
            pid: std::process::id(),
            proxy_port: started_port,
            routes: vec![RouteTarget {
                host: "app".to_string(),
                path: None,
                strip_prefix: false,
                port: free_port()?,
                service: None,
            }],
        },
        started_port,
        None,
        &shutdown,
    )
    .await?;

    // The same binary, claiming a newer release
    let launcher = ProxyLauncher::new(
        "env",
        [
            format!("{}={}", micromux::STATE_DIR_VARIABLE, state.display()),
            common::MICROMUX.to_string(),
            "proxy".to_string(),
        ],
        "99.0.0",
    );
    let session_port = free_port()?;
    let served = ensure_proxy(
        &paths,
        PortPreference {
            port: session_port,
            fallback_port: None,
        },
        Some(&launcher),
    )
    .await;

    let deadline = Instant::now() + Duration::from_secs(10);
    let exited = loop {
        if let Some(status) = old.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = old.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let successor = proxy_info(&paths.socket, |info| lists(info, "one"));
    // The successor outlives the test otherwise; without a session, it stops at once.
    shutdown.cancel();
    let deadline = Instant::now() + Duration::from_secs(10);
    while ask_proxy(&paths.socket, r#"{"type":"stop_if_idle"}"#)
        .is_ok_and(|reply| reply.get("type").and_then(serde_json::Value::as_str) == Some("busy"))
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(50));
    }

    assert_eq!(served?, session_port);
    assert!(exited.is_some(), "the old proxy kept running");
    let (successor_pid, successor_port) = successor?;
    assert_ne!(successor_pid, old_pid);
    assert_eq!(successor_port, u64::from(started_port));
    Ok(())
}
