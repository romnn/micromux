//! `micromux env` and `micromux exec`, exercised through the real binary against a headless
//! session.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixListener;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use color_eyre::eyre;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use similar_asserts::assert_eq;

const MICROMUX: &str = env!("CARGO_BIN_EXE_micromux");

/// A session with one allocated port and a service whose environment mixes an env file, an
/// interpolated inline entry that needs quoting, and a key no shell can assign.
const CONFIG: &str = r#"version: 1
allocate_ports: [EXEC_TEST_PORT]
services:
  svc:
    name: Display Name
    command: ["sh", "-c", "exec sleep 300"]
    env_file: svc.env
    environment:
      INLINE: "it's ${FROM_ENV_FILE}"
      NOT-SHELL: "x"
"#;

const ENV_FILE: &str = "FROM_ENV_FILE=from env file\n";

/// The note `--service` prints when the service's definition changed since its process started.
const STALE_NOTE: &str =
    "svc's config changed since it started; showing the environment of its running process";

fn micromux(directory: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(MICROMUX);
    command
        .args(args)
        .current_dir(directory)
        .stdin(Stdio::null());
    command
}

fn run(directory: &Path, args: &[&str]) -> eyre::Result<Output> {
    Ok(micromux(directory, args).output()?)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Parses `KEY=value` lines whose values need no quoting.
fn parse_lines(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// Stops the session when the test ends, however it ends.
struct Session {
    directory: tempfile::TempDir,
    supervisor: Child,
}

impl Session {
    fn start() -> eyre::Result<Self> {
        let directory = tempfile::tempdir()?;
        std::fs::write(directory.path().join("micromux.yaml"), CONFIG)?;
        std::fs::write(directory.path().join("svc.env"), ENV_FILE)?;
        Self::serve(directory)
    }

    /// Serves the config already written to `directory`.
    fn serve(directory: tempfile::TempDir) -> eyre::Result<Self> {
        let supervisor = Command::new(MICROMUX)
            .arg("serve")
            .current_dir(directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let session = Self {
            directory,
            supervisor,
        };
        // The control endpoint comes up shortly after the process starts.
        let deadline = Instant::now() + Duration::from_secs(30);
        while !run(session.path(), &["ctl", "ls"])?.status.success() {
            eyre::ensure!(
                Instant::now() < deadline,
                "the session never answered `ctl ls`"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(session)
    }

    fn path(&self) -> &Path {
        self.directory.path()
    }

    fn run(&self, args: &[&str]) -> eyre::Result<Output> {
        run(self.path(), args)
    }

    /// The session's allocated port, as `micromux env NAME` prints it.
    fn port(&self) -> eyre::Result<String> {
        let output = self.run(&["env", "EXEC_TEST_PORT"])?;
        eyre::ensure!(output.status.success(), "{}", stderr(&output));
        Ok(stdout(&output).trim_end().to_string())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = run(self.path(), &["ctl", "stop"]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.supervisor.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.supervisor.kill();
        let _ = self.supervisor.wait();
    }
}

/// Kills and reaps a child that a failed assertion left running.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `env` prints the session's variables or a service's resolved environment, in each format,
/// and fails without printing anything when a name is not set.
#[test]
fn env_prints_the_session_and_the_service_environment() -> eyre::Result<()> {
    let session = Session::start()?;
    let port = session.port()?;
    assert!(port.parse::<u16>().is_ok(), "{port}");

    // The session's own variables, sorted, and nothing else on either stream
    let output = session.run(&["env"])?;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    let pins = parse_lines(&stdout(&output));
    assert_eq!(
        pins.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "EXEC_TEST_PORT",
            "MICROMUX_HOST_SUFFIX",
            "MICROMUX_PROXY_PORT",
            "MICROMUX_SESSION",
            "MICROMUX_SESSION_DOMAIN",
        ]
    );
    assert_eq!(pins.get("EXEC_TEST_PORT"), Some(&port));

    // The service's env file and inline entries, quoted for a shell, with the pins applied; the
    // key no shell can assign is left out with a warning that names only the key.
    let output = session.run(&["env", "--service", "svc"])?;
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("FROM_ENV_FILE='from env file'\n"), "{text}");
    assert!(text.contains("INLINE='it'\\''s from env file'\n"), "{text}");
    assert!(text.contains(&format!("EXEC_TEST_PORT={port}\n")), "{text}");
    assert!(!text.contains("NOT-SHELL"), "{text}");
    assert!(!text.contains("TERM="), "{text}");
    assert_eq!(
        stderr(&output),
        "micromux: skipping \"NOT-SHELL\", which is not a valid shell variable name\n"
    );

    // A requested name no shell can assign fails instead of vanishing from the assignments.
    let output = session.run(&["env", "--service", "svc", "--export", "NOT-SHELL", "INLINE"])?;
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("\"NOT-SHELL\""),
        "{}",
        stderr(&output)
    );

    // JSON carries every key.
    let output = session.run(&["env", "--service", "svc", "--format", "json"])?;
    assert!(output.status.success(), "{}", stderr(&output));
    let json = serde_json::from_slice::<BTreeMap<String, String>>(&output.stdout)?;
    assert_eq!(
        json.get("INLINE").map(String::as_str),
        Some("it's from env file")
    );
    assert_eq!(json.get("NOT-SHELL").map(String::as_str), Some("x"));

    // `--export` output loads into a shell.
    let output = Command::new("sh")
        .args([
            "-c",
            r#"eval "$("$MICROMUX" env --export --service svc)" && printf '%s|%s' "$INLINE" "$EXEC_TEST_PORT""#,
        ])
        .env("MICROMUX", MICROMUX)
        .current_dir(session.path())
        .stdin(Stdio::null())
        .output()?;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("it's from env file|{port}"));

    // A name that is not set fails without printing the ones that are.
    let output = session.run(&["env", "EXEC_TEST_PORT", "MISSING_NAME"])?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("MISSING_NAME"),
        "{}",
        stderr(&output)
    );

    // `--service` takes the id, never the display name.
    for unknown in ["nope", "Display Name"] {
        let output = session.run(&["env", "--service", unknown])?;
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(stdout(&output), "");
        assert!(
            stderr(&output).contains(&format!("has no service with id `{unknown}`")),
            "{}",
            stderr(&output)
        );
    }

    // `--session` reaches the session from a directory without a config.
    let elsewhere = tempfile::tempdir()?;
    let name = pins
        .get("MICROMUX_SESSION")
        .ok_or_else(|| eyre::eyre!("MICROMUX_SESSION is missing"))?;
    let output = run(
        elsewhere.path(),
        &[
            "env",
            "--session",
            &format!("name:{name}"),
            "EXEC_TEST_PORT",
        ],
    )?;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{port}\n"));
    Ok(())
}

/// `exec` runs the command with the caller's environment plus the session's, passes its stdin,
/// arguments, and exit status through, and reports micromux's own failures with distinct codes.
#[test]
fn exec_runs_the_command_with_the_session_environment() -> eyre::Result<()> {
    let session = Session::start()?;
    let port = session.port()?;

    // The exit status passes through.
    let output = session.run(&["exec", "--", "sh", "-c", "echo \"$EXEC_TEST_PORT\"; exit 7"])?;
    assert_eq!(output.status.code(), Some(7), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{port}\n"));
    assert_eq!(stderr(&output), "");

    // A service's environment, with the command given without `--`
    let output = session.run(&[
        "exec",
        "--service",
        "svc",
        "sh",
        "-c",
        "printf '%s' \"$FROM_ENV_FILE\"",
    ])?;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "from env file");

    // The caller's variables stay, and the session's override those of the same name.
    let output = micromux(
        session.path(),
        &[
            "exec",
            "sh",
            "-c",
            "printf '%s %s' \"$CALLER_ONLY\" \"$EXEC_TEST_PORT\"",
        ],
    )
    .env("CALLER_ONLY", "kept")
    .env("EXEC_TEST_PORT", "1")
    .output()?;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("kept {port}"));

    // Flags after the command name belong to the command.
    let output = session.run(&["exec", "sh", "-c", "printf '%s' \"$1\"", "sh", "--service"])?;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "--service");

    // Stdin reaches the command.
    let mut child = micromux(
        session.path(),
        &["exec", "sh", "-c", "read line; printf '%s' \"$line\""],
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| eyre::eyre!("stdin is not piped"))?
        .write_all(b"from stdin\n")?;
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    assert_eq!(stdout(&output), "from stdin");

    // Command failures use the shell's codes, micromux's own failures 125.
    let output = session.run(&["exec", "micromux-test-no-such-command"])?;
    assert_eq!(output.status.code(), Some(127));
    assert!(
        stderr(&output).contains("cannot run `micromux-test-no-such-command`"),
        "{}",
        stderr(&output)
    );
    std::fs::write(session.path().join("not-executable"), "#!/bin/sh\n")?;
    let output = session.run(&["exec", "./not-executable"])?;
    assert_eq!(output.status.code(), Some(126), "{}", stderr(&output));
    let output = session.run(&["exec", "--service", "nope", "--", "true"])?;
    assert_eq!(output.status.code(), Some(125));
    assert!(
        stderr(&output).contains("has no service with id `nope`"),
        "{}",
        stderr(&output)
    );
    Ok(())
}

/// The command replaces micromux, so it keeps its pid and gets the caller's signals directly.
#[test]
fn exec_replaces_the_micromux_process() -> eyre::Result<()> {
    let session = Session::start()?;

    let mut child = Reaped(
        micromux(
            session.path(),
            &["exec", "sh", "-c", "echo $$; exec sleep 30"],
        )
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn()?,
    );
    let mut line = String::new();
    BufReader::new(
        child
            .0
            .stdout
            .take()
            .ok_or_else(|| eyre::eyre!("stdout is not piped"))?,
    )
    .read_line(&mut line)?;
    assert_eq!(line.trim_end(), child.0.id().to_string());

    let pid = Pid::from_raw(i32::try_from(child.0.id())?);
    kill(pid, Signal::SIGTERM)?;
    let status = child.0.wait()?;
    assert_eq!(status.signal(), Some(Signal::SIGTERM as i32));

    // Every descriptor micromux opened closes on exec, so the command holds exactly what a shell
    // started directly with the same stdio holds.
    #[cfg(target_os = "linux")]
    {
        let list_descriptors = ["sh", "-c", "ls /proc/$$/fd"];
        let direct = Command::new(list_descriptors[0])
            .args(&list_descriptors[1..])
            .stdin(Stdio::null())
            .output()?;
        let through_exec = session.run(&[&["exec", "--"], &list_descriptors[..]].concat())?;
        assert!(through_exec.status.success(), "{}", stderr(&through_exec));
        assert_eq!(stdout(&through_exec), stdout(&direct));
    }
    Ok(())
}

/// After a reconcile changed the service, both commands keep showing what its running process
/// got, with a note that never shows a value.
#[test]
fn a_changed_service_reports_its_running_environment_with_a_note() -> eyre::Result<()> {
    let session = Session::start()?;
    std::fs::write(
        session.path().join("micromux.yaml"),
        CONFIG.replace("it's ${FROM_ENV_FILE}", "changed"),
    )?;
    let output = session.run(&["ctl", "reconcile"])?;
    assert!(output.status.success(), "{}", stderr(&output));

    let output = session.run(&["env", "--service", "svc", "INLINE"])?;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "it's from env file\n");
    assert_eq!(stderr(&output), format!("micromux: {STALE_NOTE}\n"));

    let output = session.run(&[
        "exec",
        "--service",
        "svc",
        "--",
        "sh",
        "-c",
        "printf '%s' \"$INLINE\"",
    ])?;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "it's from env file");
    assert_eq!(stderr(&output), format!("micromux: {STALE_NOTE}\n"));
    Ok(())
}

/// A reconcile after a change to the env file's contents alone marks the service stale in
/// `ctl ls` exactly as `env` reports it, before and after the restart that applies it.
#[test]
fn ctl_and_env_agree_on_a_changed_env_file() -> eyre::Result<()> {
    let session = Session::start()?;
    let svc_row = |session: &Session| -> eyre::Result<String> {
        let output = session.run(&["ctl", "ls"])?;
        eyre::ensure!(output.status.success(), "{}", stderr(&output));
        stdout(&output)
            .lines()
            .find(|line| line.contains("id=svc "))
            .map(str::to_string)
            .ok_or_else(|| eyre::eyre!("no svc row in `ctl ls`"))
    };

    // A fresh run is current everywhere.
    assert!(!svc_row(&session)?.contains("config=stale"));

    std::fs::write(
        session.path().join("svc.env"),
        "FROM_ENV_FILE=changed in env file\n",
    )?;
    let output = session.run(&["ctl", "reconcile"])?;
    assert!(output.status.success(), "{}", stderr(&output));

    // Both commands call the running process stale, and `env` still shows what it got.
    let row = svc_row(&session)?;
    assert!(row.contains("config=stale"), "{row}");
    let output = session.run(&["env", "--service", "svc", "INLINE"])?;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "it's from env file\n");
    assert_eq!(stderr(&output), format!("micromux: {STALE_NOTE}\n"));

    // The restart applies the new contents, and both commands agree it is current again.
    let output = session.run(&["ctl", "restart", "svc"])?;
    assert!(output.status.success(), "{}", stderr(&output));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let row = svc_row(&session)?;
        if row.contains("generation=2") && row.contains("execution=Running") {
            assert!(!row.contains("config=stale"), "{row}");
            break;
        }
        eyre::ensure!(Instant::now() < deadline, "svc never restarted: {row}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = session.run(&["env", "--service", "svc", "INLINE"])?;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "it's changed in env file\n");
    assert_eq!(stderr(&output), "");
    Ok(())
}

/// Without a session for the exact config, both commands fail before running anything, even
/// when a session runs for a config further down the directory tree.
#[test]
fn env_and_exec_fail_without_a_session_for_the_config() -> eyre::Result<()> {
    let parent = tempfile::tempdir()?;
    std::fs::write(parent.path().join("micromux.yaml"), CONFIG)?;
    std::fs::write(parent.path().join("svc.env"), ENV_FILE)?;
    let child = tempfile::tempdir_in(parent.path())?;
    std::fs::write(child.path().join("micromux.yaml"), CONFIG)?;
    std::fs::write(child.path().join("svc.env"), ENV_FILE)?;
    let _running = Session::serve(child)?;

    let output = run(parent.path(), &["env"])?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("no running micromux session for"),
        "{}",
        stderr(&output)
    );

    let output = run(parent.path(), &["exec", "--", "sh", "-c", "touch ran"])?;
    assert_eq!(output.status.code(), Some(125));
    assert!(
        stderr(&output).contains("no running micromux session for"),
        "{}",
        stderr(&output)
    );
    assert!(!parent.path().join("ran").exists());
    Ok(())
}

/// A stand-in for a session started by a micromux that speaks protocol 3.13: it describes itself
/// and rejects every other request, as such a session rejects `GetEnvironment`.
struct OldSession {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl OldSession {
    fn bind(directory: &Path) -> eyre::Result<Self> {
        let config = micromux_control::CanonicalConfigPath::new(directory.join("micromux.yaml"))?;
        let runtime_dir =
            micromux_control::usable_runtime_dirs(&micromux_control::runtime_dir_statuses())
                .into_iter()
                .next()
                .ok_or_else(|| eyre::eyre!("no usable runtime directory"))?;
        let micromux_control::ControlEndpoint::Unix(socket) =
            micromux_control::endpoint_for(&runtime_dir, &config)
        else {
            eyre::bail!("expected a Unix socket endpoint");
        };
        let description = micromux_control::Response::Description(micromux_control::SessionInfo {
            protocol_version: micromux_control::ProtocolVersion::new(3, 13),
            id: micromux_control::endpoint_hash(&config),
            pid: std::process::id(),
            start_time: 1,
            name: "old".to_string(),
            working_dir: directory.to_string_lossy().into_owned(),
            config_path: config.to_string_lossy().into_owned(),
            services: Vec::new(),
            services_truncated: false,
            micromux_version: "0.0.0".to_string(),
            session: None,
            capabilities: None,
        });
        let description = serde_json::to_string(&description)?;
        let rejection = serde_json::to_string(&micromux_control::Response::error(
            micromux_control::ErrorCode::BadRequest,
            "unknown variant",
        ))?;

        let listener = UnixListener::bind(&socket)?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let server = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                while !stop.load(Ordering::Relaxed) {
                    let Ok((stream, _)) = listener.accept() else {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    };
                    let _ = answer(stream, &description, &rejection);
                }
            }
        });
        Ok(Self {
            socket,
            stop,
            server: Some(server),
        })
    }
}

/// Answers every request on one connection until the client hangs up.
fn answer(
    stream: std::os::unix::net::UnixStream,
    description: &str,
    rejection: &str,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut writer = stream.try_clone()?;
    for request in BufReader::new(stream).lines() {
        let reply = if request? == "\"Describe\"" {
            description
        } else {
            rejection
        };
        writeln!(writer, "{reply}")?;
    }
    Ok(())
}

impl Drop for OldSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// A session too old to answer is refused before anything is asked of it or run.
#[test]
fn env_and_exec_refuse_a_session_too_old_to_answer() -> eyre::Result<()> {
    let project = tempfile::tempdir()?;
    std::fs::write(
        project.path().join("micromux.yaml"),
        "version: 1\nservices: {}\n",
    )?;
    let _old = OldSession::bind(project.path())?;

    let output = run(project.path(), &["env"])?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains(
            "the running session `old` (protocol 3.13) is too old for `micromux env`; restart it \
             with this micromux"
        ),
        "{}",
        stderr(&output)
    );

    let output = run(project.path(), &["exec", "--", "sh", "-c", "touch ran"])?;
    assert_eq!(output.status.code(), Some(125));
    assert!(
        stderr(&output).contains("is too old for `micromux exec`"),
        "{}",
        stderr(&output)
    );
    assert!(!project.path().join("ran").exists());
    Ok(())
}
