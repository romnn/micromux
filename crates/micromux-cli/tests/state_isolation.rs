//! `MICROMUX_STATE_DIR` isolation, exercised through the real binary.
#![cfg(unix)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

use color_eyre::eyre;
use similar_asserts::assert_eq;

mod common;

/// A session that uses every piece of per-user state: an allocated port in the registry, a
/// cleanup, and a route through the shared proxy.
fn config(proxy_port: u16) -> String {
    format!(
        r#"version: 1
allocate_ports: [ISOLATED_PORT]
proxy:
  port: {proxy_port}
routes:
  - host: app
    port: ${{ISOLATED_PORT}}
services:
  svc:
    command: ["sh", "-c", "exec sleep 300"]
    cleanup:
      command: ["sh", "-c", "echo cleaned"]
"#
    )
}

fn run(directory: &Path, args: &[&str]) -> eyre::Result<Output> {
    Ok(common::micromux(directory)
        .args(args)
        .current_dir(directory)
        .stdin(Stdio::null())
        .output()?)
}

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

/// Stops the isolated proxy when the test ends, however it ends.
///
/// The proxy outlives the session that started it by a minute, and this one belongs to the test.
struct StopProxy(PathBuf);

impl Drop for StopProxy {
    fn drop(&mut self) {
        // The session's registration ends with its connection, so the proxy turns idle shortly
        // after the session stopped.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match ask_proxy(&self.0, r#"{"type":"stop_if_idle"}"#) {
                Ok(reply)
                    if reply.get("type").and_then(serde_json::Value::as_str) == Some("busy") =>
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => return,
            }
        }
    }
}

/// Stops the session when the test ends, however it ends.
struct Session {
    directory: PathBuf,
    supervisor: Child,
}

impl Session {
    fn stop(&mut self) -> eyre::Result<()> {
        let _ = run(&self.directory, &["ctl", "stop"]);
        let deadline = Instant::now() + Duration::from_secs(20);
        while self.supervisor.try_wait()?.is_none() {
            eyre::ensure!(Instant::now() < deadline, "the session never stopped");
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.stop().is_err() {
            let _ = self.supervisor.kill();
            let _ = self.supervisor.wait();
        }
    }
}

/// With `MICROMUX_STATE_DIR` set, everything a session shares per user lands in that directory:
/// its port record and the registry lock, its control socket and session lock, the shared proxy
/// it registers its routes with, its cleanup files, its run-log spool, and its logs.
///
/// Nothing reaches the user's own registry, sessions, or proxy, which the session would otherwise
/// share with every session the developer runs.
#[test]
fn a_session_keeps_all_its_state_in_the_state_directory() -> eyre::Result<()> {
    assert_session_state(ProxyPortSource::Config)
}

#[test]
fn proxy_port_environment_overrides_config() -> eyre::Result<()> {
    assert_session_state(ProxyPortSource::Environment)
}

#[test]
fn proxy_port_flag_overrides_environment_and_config() -> eyre::Result<()> {
    assert_session_state(ProxyPortSource::Flag)
}

#[test]
fn proxy_port_override_keeps_configured_fallback() -> eyre::Result<()> {
    assert_session_state(ProxyPortSource::Fallback)
}

#[derive(Clone, Copy)]
enum ProxyPortSource {
    Config,
    Environment,
    Flag,
    Fallback,
}

impl ProxyPortSource {
    fn prepare(
        self,
        command: &mut std::process::Command,
        proxy_port: u16,
        occupied_port: u16,
    ) -> String {
        let configured_port = if matches!(self, ProxyPortSource::Config) {
            proxy_port
        } else {
            occupied_port
        };
        let mut yaml = config(configured_port);
        if matches!(self, ProxyPortSource::Fallback) {
            yaml = yaml.replace(
                &format!("port: {configured_port}"),
                &format!("port: {configured_port}\n  fallback_port: {proxy_port}"),
            );
        }

        command.arg("serve").env_remove("MICROMUX_PROXY_PORT");
        match self {
            ProxyPortSource::Config => {}
            ProxyPortSource::Environment => {
                command.env("MICROMUX_PROXY_PORT", proxy_port.to_string());
            }
            ProxyPortSource::Flag => {
                command
                    .env("MICROMUX_PROXY_PORT", occupied_port.to_string())
                    .args(["--proxy-port", &proxy_port.to_string()]);
            }
            ProxyPortSource::Fallback => {
                command.args(["--proxy-port", &occupied_port.to_string()]);
            }
        }
        yaml
    }
}

fn assert_session_state(source: ProxyPortSource) -> eyre::Result<()> {
    let project = tempfile::tempdir()?;
    let directory = project.path();
    let state = common::state_directory(directory);
    let proxy_port = free_port()?;
    let occupied = std::net::TcpListener::bind("127.0.0.1:0")?;
    let occupied_port = occupied.local_addr()?.port();
    let mut command = common::micromux(directory);
    let yaml = source.prepare(&mut command, proxy_port, occupied_port);
    std::fs::write(directory.join("micromux.yaml"), &yaml)?;
    let config_path = directory.join("micromux.yaml").canonicalize()?;
    let key = micromux::session::config_path_key(&config_path);
    let runtime_dir = micromux_control::runtime_dir_in(&state);
    let coordination_dir = micromux_control::coordination_dir_in(&state);

    let _proxy = StopProxy(coordination_dir.join("proxy.sock"));
    let supervisor = command
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let mut session = Session {
        directory: directory.to_path_buf(),
        supervisor,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !run(directory, &["ctl", "ls"])?.status.success() {
        eyre::ensure!(
            Instant::now() < deadline,
            "the session never answered `ctl ls`"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    assert_port_registry(directory, proxy_port)?;

    // The control socket and the session lock live in the state directory too, and so does the
    // session's run-log spool.
    assert!(runtime_dir.join(format!("{key}.sock")).exists());
    assert!(
        coordination_dir
            .join(format!("{key}.session.lock"))
            .exists()
    );
    let spool_prefix = format!("{}-", session.supervisor.id());
    let spools = std::fs::read_dir(state.join("cache").join("session-run-logs"))?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(&spool_prefix)
        })
        .count();
    assert_eq!(spools, 1);

    // The session started a proxy of its own on the configured port and registered with it,
    // rather than joining the developer's proxy.
    let info = ask_proxy(&coordination_dir.join("proxy.sock"), r#"{"type":"hello"}"#)?;
    assert_eq!(info.get("port"), Some(&serde_json::json!(proxy_port)));
    assert_eq!(
        info.pointer("/sessions/0/config_path")
            .and_then(serde_json::Value::as_str),
        config_path.to_str()
    );

    assert_service_proxy_environment(directory, proxy_port)?;

    // A run-local override must not make unchanged YAML look like changed session settings.
    for arguments in [
        vec!["ctl", "reconcile", "--dry-run"],
        vec!["ctl", "reconcile"],
    ] {
        let output = run(directory, &arguments)?;
        assert!(output.status.success(), "{output:?}");
    }
    assert_eq!(std::fs::read_to_string(&config_path)?, yaml);

    // Stopping the session runs the cleanup, whose log lands in the state directory, and the
    // session logged there as well.
    session.stop()?;
    let cleanup = micromux::cleanup_directory_in(&state, &config_path);
    let cleanup_log = std::fs::read_to_string(cleanup.join("svc.log"))?;
    assert!(cleanup_log.contains("cleaned"), "{cleanup_log}");
    let logs = std::fs::read_dir(state.join("cache"))?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("micromux.log")
        })
        .count();
    assert!(logs > 0, "the session wrote no log in the state directory");
    Ok(())
}

fn assert_port_registry(directory: &Path, proxy_port: u16) -> eyre::Result<()> {
    let state = common::state_directory(directory);
    let key = micromux::session::config_path_key(&directory.join("micromux.yaml").canonicalize()?);
    // The registry in the state directory holds the session's port, and its lock names no holder
    // once the session finished allocating.
    let output = run(directory, &["env", "ISOLATED_PORT", "MICROMUX_PROXY_PORT"])?;
    eyre::ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let printed = String::from_utf8(output.stdout)?;
    let variable = |name: &str| {
        printed
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .ok_or_else(|| eyre::eyre!("`env` printed no {name}: {printed}"))
    };
    let port = variable("ISOLATED_PORT")?;
    assert_eq!(variable("MICROMUX_PROXY_PORT")?, proxy_port.to_string());
    let record: serde_json::Value = serde_json::from_slice(&std::fs::read(
        state.join("ports").join(format!("{key}.json")),
    )?)?;
    assert_eq!(
        record
            .pointer("/ports/ISOLATED_PORT")
            .ok_or_else(|| eyre::eyre!("missing port record"))?
            .to_string(),
        port
    );
    assert_eq!(
        std::fs::read_to_string(state.join("ports").join("registry.lock"))?,
        ""
    );

    Ok(())
}

fn assert_service_proxy_environment(directory: &Path, proxy_port: u16) -> eyre::Result<()> {
    // Services and URLs use the effective port, including a fallback after an override.
    let output = run(directory, &["env", "--service", "svc", "--format", "json"])?;
    assert!(output.status.success(), "{output:?}");
    let environment: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        environment
            .get("MICROMUX_PROXY_PORT")
            .and_then(serde_json::Value::as_str),
        Some(proxy_port.to_string().as_str())
    );
    let suffix = environment
        .get("MICROMUX_HOST_SUFFIX")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| eyre::eyre!("missing host suffix"))?;
    assert!(suffix.ends_with(&format!(":{proxy_port}")), "{suffix}");

    Ok(())
}

/// A relative state directory would name a different setup in every directory micromux starts
/// in, so micromux refuses it before doing anything.
#[test]
fn a_relative_state_directory_is_refused() -> eyre::Result<()> {
    let project = tempfile::tempdir()?;
    std::fs::write(
        project.path().join("micromux.yaml"),
        "version: 1\nservices: {}\n",
    )?;

    let output = common::micromux(project.path())
        .env(micromux::STATE_DIR_VARIABLE, "state")
        .args(["ctl", "ls"])
        .current_dir(project.path())
        .stdin(Stdio::null())
        .output()?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("MICROMUX_STATE_DIR must be an absolute path"),
        "{stderr}"
    );
    // Nothing was created under the relative path.
    assert!(!project.path().join("state").exists());
    Ok(())
}
