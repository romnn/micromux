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

mod common;

use common::MICROMUX;

/// A session with one allocated port, a session-wide env file, and a service whose environment
/// mixes its own env file, an interpolated inline entry that needs quoting, and a key no shell
/// can assign.
const CONFIG: &str = r#"version: 1
allocate_ports: [EXEC_TEST_PORT]
env_file: session.env
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

/// The session-wide env file, whose own definition of the allocated port loses to the session's.
const SESSION_ENV_FILE: &str =
    "SHARED=from session file\nEXEC_TEST_PORT=1\nSESSION_URL=http://127.0.0.1:${EXEC_TEST_PORT}/\n";

/// Writes the project's config and env files to `directory`.
fn write_project(directory: &Path) -> eyre::Result<()> {
    std::fs::write(directory.join("micromux.yaml"), CONFIG)?;
    std::fs::write(directory.join("svc.env"), ENV_FILE)?;
    std::fs::write(directory.join("session.env"), SESSION_ENV_FILE)?;
    Ok(())
}

/// The note `--service` prints when the service's definition changed since its process started.
const STALE_NOTE: &str =
    "svc's config changed since it started; showing the environment of its running process";

fn micromux(directory: &Path, args: &[&str]) -> Command {
    let mut command = common::micromux(directory);
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
        write_project(directory.path())?;
        Self::serve(directory)
    }

    /// Serves the config already written to `directory`.
    fn serve(directory: tempfile::TempDir) -> eyre::Result<Self> {
        let supervisor = common::micromux(directory.path())
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

    /// Stops the session the way `ctl stop` does and waits until its process exited.
    fn stop(&mut self) -> eyre::Result<()> {
        let _ = run(self.path(), &["ctl", "stop"]);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.supervisor.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.supervisor.kill()?;
        self.supervisor.wait()?;
        eyre::bail!("the session did not stop on `ctl stop`")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
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

/// `env` prints the session's own environment or a service's resolved environment, in each
/// format, and fails without printing anything when a name is not set.
#[test]
fn env_prints_the_session_and_the_service_environment() -> eyre::Result<()> {
    let session = Session::start()?;
    let port = session.port()?;
    assert!(port.parse::<u16>().is_ok(), "{port}");

    // The session's own environment, sorted: its variables over the session-wide env file, whose
    // reference to the allocated port sees the allocated value, and nothing else on either stream
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
            "SESSION_URL",
            "SHARED",
        ]
    );
    assert_eq!(pins.get("EXEC_TEST_PORT"), Some(&port));
    assert_eq!(
        pins.get("SESSION_URL"),
        Some(&format!("http://127.0.0.1:{port}/"))
    );

    // The session-wide and the service's own env file and inline entries, quoted for a shell,
    // with the pins applied; the key no shell can assign is left out with a warning that names
    // only the key.
    let output = session.run(&["env", "--service", "svc"])?;
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("FROM_ENV_FILE='from env file'\n"), "{text}");
    assert!(text.contains("SHARED='from session file'\n"), "{text}");
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
    let output = common::isolated("sh", session.path())
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

    // `--session` reaches the session from a directory without a config, as long as it shares
    // the session's state.
    let elsewhere = tempfile::tempdir()?;
    let name = pins
        .get("MICROMUX_SESSION")
        .ok_or_else(|| eyre::eyre!("MICROMUX_SESSION is missing"))?;
    let output = common::micromux(session.path())
        .args([
            "env",
            "--session",
            &format!("name:{name}"),
            "EXEC_TEST_PORT",
        ])
        .current_dir(elsewhere.path())
        .stdin(Stdio::null())
        .output()?;
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
    write_project(parent.path())?;
    let child = tempfile::tempdir_in(parent.path())?;
    write_project(child.path())?;
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

/// A stand-in for a session started by an older micromux: it describes itself with the protocol
/// it is given and rejects every other request, as a 3.13 session rejects `GetEnvironment`.
struct OldSession {
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl OldSession {
    fn bind(
        directory: &Path,
        protocol_version: micromux_control::ProtocolVersion,
    ) -> eyre::Result<Self> {
        let config = micromux_control::CanonicalConfigPath::new(directory.join("micromux.yaml"))?;
        // Where the isolated micromux the test runs looks for sessions.
        let runtime_dir = micromux_control::runtime_dir_in(&common::state_directory(directory));
        {
            use std::os::unix::fs::DirBuilderExt as _;

            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&runtime_dir)?;
        }
        let micromux_control::ControlEndpoint::Unix(socket) =
            micromux_control::endpoint_for(&runtime_dir, &config)
        else {
            eyre::bail!("expected a Unix socket endpoint");
        };
        let description = micromux_control::Response::Description(micromux_control::SessionInfo {
            protocol_version,
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
    let _old = OldSession::bind(
        project.path(),
        micromux_control::ProtocolVersion::new(3, 13),
    )?;

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

/// A session that answers `GetEnvironment` but predates the session-wide env file would answer
/// with its variables alone, so the session's environment is refused, written or not, and
/// `--if-running` does not excuse it.
#[test]
fn the_session_environment_refuses_a_session_without_session_env_files() -> eyre::Result<()> {
    let project = tempfile::tempdir()?;
    std::fs::write(
        project.path().join("micromux.yaml"),
        "version: 1\nservices: {}\n",
    )?;
    let _old = OldSession::bind(
        project.path(),
        micromux_control::ProtocolVersion::new(3, 19),
    )?;

    for (args, command) in [
        (&["env"][..], "env"),
        (&["env", "--if-running"], "env"),
        (&["env", "--file"], "env --file"),
        (&["env", "--file", "--service", "svc"], "env --file"),
    ] {
        let output = run(project.path(), args)?;
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert_eq!(stdout(&output), "", "{args:?}");
        assert!(
            stderr(&output).contains(&format!(
                "the running session `old` (protocol 3.19) is too old for `micromux {command}`"
            )),
            "{args:?}: {}",
            stderr(&output)
        );
    }

    let output = run(
        project.path(),
        &["exec", "--if-running", "--", "sh", "-c", "touch ran"],
    )?;
    assert_eq!(output.status.code(), Some(125));
    assert!(!project.path().join("ran").exists());
    Ok(())
}

/// The mode bits of `path`.
fn mode(path: &Path) -> eyre::Result<u32> {
    use std::os::unix::fs::PermissionsExt as _;

    Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
}

/// The path `micromux env --file` prints, after checking it printed nothing else.
fn env_file(session: &Session, args: &[&str]) -> eyre::Result<PathBuf> {
    let output = session.run(&[&["env", "--file"], args].concat())?;
    eyre::ensure!(output.status.success(), "{}", stderr(&output));
    let printed = stdout(&output);
    let path = printed
        .strip_suffix('\n')
        .ok_or_else(|| eyre::eyre!("no newline after the path: {printed:?}"))?;
    eyre::ensure!(!path.contains('\n'), "more than a path: {printed:?}");
    Ok(PathBuf::from(path))
}

/// `env --file` has the session write its environment, or a service's, to a private dotenv file
/// next to its control socket, prints the absolute path, and rewrites the same file each time.
#[test]
fn env_file_writes_a_private_dotenv_file_the_session_owns() -> eyre::Result<()> {
    let session = Session::start()?;
    let port = session.port()?;

    let path = env_file(&session, &[])?;
    assert!(path.is_absolute(), "{}", path.display());
    let runtime_dir = micromux_control::runtime_dir_in(&common::state_directory(session.path()));
    assert_eq!(
        path.parent().and_then(Path::parent),
        Some(runtime_dir.as_path())
    );
    assert_eq!(mode(&path)?, 0o600);
    assert_eq!(
        mode(path.parent().ok_or_else(|| eyre::eyre!("no parent"))?)?,
        0o700
    );
    let contents = std::fs::read_to_string(&path)?;
    for line in [
        format!("EXEC_TEST_PORT=\"{port}\""),
        format!("SESSION_URL=\"http://127.0.0.1:{port}/\""),
        "SHARED=\"from session file\"".to_string(),
    ] {
        assert!(
            contents.lines().any(|written| written == line),
            "{contents}"
        );
    }
    assert!(contents.contains("MICROMUX_SESSION="), "{contents}");

    // The same file each time
    assert_eq!(env_file(&session, &[])?, path);

    // A service's environment goes to its own file; the key no dotenv reader takes is left out
    // with a warning that names only the key.
    let output = session.run(&["env", "--file", "--service", "svc"])?;
    assert!(output.status.success(), "{}", stderr(&output));
    let service_path = PathBuf::from(stdout(&output).trim_end());
    assert_ne!(service_path, path);
    assert_eq!(service_path.parent(), path.parent());
    assert_eq!(
        stderr(&output),
        "micromux: skipping \"NOT-SHELL\", which is not a valid shell variable name\n"
    );
    let contents = std::fs::read_to_string(&service_path)?;
    assert!(
        contents
            .lines()
            .any(|line| line == "INLINE=\"it's from env file\""),
        "{contents}"
    );
    assert!(!contents.contains("NOT-SHELL"), "{contents}");

    // An unknown service fails without writing anything.
    let output = session.run(&["env", "--file", "--service", "nope"])?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("has no service with id `nope`"),
        "{}",
        stderr(&output)
    );
    Ok(())
}

/// The session's environment reads its env file from disk on every request, so an edit shows in
/// `env`, `env --file`, and `exec` without a restart, while a running service keeps what it got.
#[test]
fn the_session_environment_reads_its_env_file_fresh() -> eyre::Result<()> {
    let session = Session::start()?;
    let path = env_file(&session, &[])?;

    std::fs::write(
        session.path().join("session.env"),
        "SHARED=edited\nADDED=since the session started\n",
    )?;

    let output = session.run(&["env", "SHARED", "ADDED"])?;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "ADDED='since the session started'\nSHARED=edited\n"
    );
    assert_eq!(env_file(&session, &[])?, path);
    let contents = std::fs::read_to_string(&path)?;
    assert!(
        contents.lines().any(|line| line == "SHARED=\"edited\""),
        "{contents}"
    );
    assert!(!contents.contains("SESSION_URL"), "{contents}");
    let output = session.run(&["exec", "--", "sh", "-c", "printf '%s' \"$ADDED\""])?;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "since the session started");

    // The running service keeps the value it started with.
    let output = session.run(&["env", "--service", "svc", "SHARED"])?;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "from session file\n");

    // A file that no longer loads fails the session's environment and says why.
    std::fs::remove_file(session.path().join("session.env"))?;
    let output = session.run(&["env"])?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("cannot load top-level env_file[0]"),
        "{}",
        stderr(&output)
    );
    Ok(())
}

/// The session's environment files go with the session, and are not left behind by one that
/// stops.
#[test]
fn the_env_file_is_removed_when_the_session_stops() -> eyre::Result<()> {
    let mut session = Session::start()?;
    let path = env_file(&session, &[])?;
    let service_path = env_file(&session, &["--service", "svc"])?;
    let directory = path
        .parent()
        .ok_or_else(|| eyre::eyre!("no parent"))?
        .to_path_buf();
    assert!(path.is_file() && service_path.is_file());

    session.stop()?;

    assert!(!directory.exists(), "{} survived", directory.display());
    Ok(())
}

/// `--if-running` skips only when no session claims the project: `env` prints nothing and
/// succeeds, and `exec` runs the command with the caller's environment unchanged.
#[test]
fn if_running_skips_only_without_a_session() -> eyre::Result<()> {
    let project = tempfile::tempdir()?;
    write_project(project.path())?;

    for args in [
        &["env", "--if-running"][..],
        &["env", "--if-running", "--file"],
        &["env", "--if-running", "--service", "svc", "INLINE"],
    ] {
        let output = run(project.path(), args)?;
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        assert_eq!(
            (stdout(&output), stderr(&output)),
            (String::new(), String::new())
        );
    }
    let output = micromux(
        project.path(),
        &[
            "exec",
            "--if-running",
            "--",
            "sh",
            "-c",
            "printf '%s %s' \"${EXEC_TEST_PORT-unset}\" \"$CALLER\"; exit 7",
        ],
    )
    .env("CALLER", "kept")
    .env_remove("EXEC_TEST_PORT")
    .output()?;
    assert_eq!(output.status.code(), Some(7), "{}", stderr(&output));
    assert_eq!(stdout(&output), "unset kept");
    assert_eq!(stderr(&output), "");

    // Without the flag, the same calls fail as before.
    let output = run(project.path(), &["env"])?;
    assert_eq!(output.status.code(), Some(1));

    // With a session, the flag changes nothing, and its failures still fail.
    let session = Session::serve(project)?;
    let port = session.port()?;
    let output = session.run(&["env", "--if-running", "EXEC_TEST_PORT"])?;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{port}\n"));
    let output = session.run(&[
        "exec",
        "--if-running",
        "--",
        "sh",
        "-c",
        "printf '%s' \"$EXEC_TEST_PORT\"",
    ])?;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), port);
    let output = session.run(&["env", "--if-running", "--service", "nope"])?;
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("has no service with id `nope`"),
        "{}",
        stderr(&output)
    );
    Ok(())
}

/// A session that claims the project without answering, because it is still starting or runs
/// without a control plane, is a failure for `--if-running`, never a reason to skip.
#[test]
fn if_running_fails_when_a_session_claims_the_project_without_answering() -> eyre::Result<()> {
    let project = tempfile::tempdir()?;
    write_project(project.path())?;
    // The owner lock a session takes before anything else, in the project's private state.
    let config = micromux_control::CanonicalConfigPath::new(project.path().join("micromux.yaml"))?;
    let coordination =
        micromux_control::coordination_dir_in(&common::state_directory(project.path()));
    {
        use std::os::unix::fs::DirBuilderExt as _;

        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&coordination)?;
    }
    let _claim = micromux_control::claim_project_at(&coordination.join(format!(
        "{}.session.lock",
        micromux_control::endpoint_hash(&config)
    )))?;

    let output = run(project.path(), &["env", "--if-running"])?;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).contains("so --if-running does not skip it"),
        "{}",
        stderr(&output)
    );

    let output = run(
        project.path(),
        &["exec", "--if-running", "--", "sh", "-c", "touch ran"],
    )?;
    assert_eq!(output.status.code(), Some(125));
    assert!(!project.path().join("ran").exists());
    Ok(())
}
