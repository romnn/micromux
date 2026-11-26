//! Replacing the shared proxy with a newer release, exercised with the real binary.
#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use color_eyre::eyre;
use micromux_proxy::{PortPreference, ProxyLauncher, ProxyPaths, ensure_proxy};
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

/// The pid and startup port of the proxy answering on `socket`, once one answers.
fn proxy_info(socket: &Path) -> eyre::Result<(u64, u64)> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(info) = ask_proxy(socket, r#"{"type":"hello"}"#)
            && let Some(pid) = info.get("pid").and_then(serde_json::Value::as_u64)
            && let Some(port) = info.get("port").and_then(serde_json::Value::as_u64)
        {
            return Ok((pid, port));
        }
        eyre::ensure!(Instant::now() < deadline, "no proxy answered");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A session whose launcher starts a newer release than the running proxy replaces it: the
/// successor takes over the old proxy's port, the old proxy exits, and the session's own port is
/// served by the successor.
#[tokio::test]
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
    let (old_pid, _) = proxy_info(&paths.socket)?;
    assert_eq!(old_pid, u64::from(old.id()));

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
    let successor = proxy_info(&paths.socket);
    // The successor outlives the test otherwise; with no session registered, it stops at once.
    let _ = ask_proxy(&paths.socket, r#"{"type":"stop_if_idle"}"#);

    assert_eq!(served?, session_port);
    assert!(exited.is_some(), "the old proxy kept running");
    let (successor_pid, successor_port) = successor?;
    assert_ne!(successor_pid, old_pid);
    assert_eq!(successor_port, u64::from(started_port));
    Ok(())
}
