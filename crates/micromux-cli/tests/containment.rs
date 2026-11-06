//! Supervisor-death containment, exercised through the real binary.
#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use color_eyre::eyre;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use similar_asserts::assert_eq;
use std::os::unix::process::CommandExt as _;

const MICROMUX: &str = env!("CARGO_BIN_EXE_micromux");

/// A `micromux __shim` invocation for `command`, which travels in the environment as the
/// supervisor passes it.
fn shim(supervisor: u32, command: &[&str]) -> Command {
    let mut shim = Command::new(MICROMUX);
    shim.args(["__shim", "--supervisor-pid", &supervisor.to_string()]);
    for (index, arg) in command.iter().enumerate() {
        shim.env(format!("MICROMUX_SHIM_ARG_{index}"), arg);
    }
    shim
}

fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

fn read_pid(path: &Path, deadline: Instant) -> eyre::Result<i32> {
    loop {
        // The shell creates the file before it writes the pid.
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse()
        {
            return Ok(pid);
        }
        eyre::ensure!(
            Instant::now() < deadline,
            "{} was never written",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Starts `micromux serve` in `directory`, in its own process group so its signals never reach
/// the test runner.
fn serve(program: &Path, directory: &Path) -> eyre::Result<std::process::Child> {
    Ok(Command::new(program)
        .arg("serve")
        .current_dir(directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?)
}

/// Reads every pid file, killing `supervisor` when one never appears.
fn read_pids(
    directory: &Path,
    names: &[&str],
    supervisor: &mut std::process::Child,
) -> eyre::Result<Vec<i32>> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let pids = names
        .iter()
        .map(|name| read_pid(&directory.join(name), deadline))
        .collect::<eyre::Result<Vec<_>>>();
    if pids.is_err() {
        let _ = supervisor.kill();
        let _ = supervisor.wait();
    }
    pids
}

/// A supervisor killed with `SIGKILL` takes its services' whole process trees with it: the
/// service's own process, a child in its process group, a child that detached into a session of
/// its own, and on Linux a detached process whose parent already exited.
#[test]
fn sigkilled_supervisor_takes_service_trees_with_it() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    // `perl` stands in for `setsid(1)`, which stock macOS lacks.
    std::fs::write(
        directory.path().join("tree.sh"),
        r#"detach() {
    perl -MPOSIX -e 'POSIX::setsid(); open(my $f, ">", $ARGV[0]); print $f $$; close $f; exec "sleep", "300"' "$1"
}
detach detached.pid &
sh -c 'echo $$ > grouped.pid; exec sleep 300' &
if [ "$(uname)" = Linux ]; then
    (detach orphan.pid &)
fi
echo $$ > leader.pid
wait
"#,
    )?;
    std::fs::write(
        directory.path().join("micromux.yaml"),
        "version: 1\nservices:\n  tree:\n    command: [\"sh\", \"./tree.sh\"]\n",
    )?;
    let mut names = vec!["detached.pid", "grouped.pid", "leader.pid"];
    if cfg!(target_os = "linux") {
        names.push("orphan.pid");
    }

    let mut supervisor = serve(Path::new(MICROMUX), directory.path())?;
    let pids = read_pids(directory.path(), &names, &mut supervisor)?;
    supervisor.kill()?;
    supervisor.wait()?;

    let deadline = Instant::now() + Duration::from_secs(20);
    while pids.iter().copied().any(alive) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let survivors = pids
        .iter()
        .copied()
        .filter(|pid| alive(*pid))
        .collect::<Vec<_>>();
    for pid in &survivors {
        let _ = kill(Pid::from_raw(*pid), Signal::SIGKILL);
    }
    eyre::ensure!(
        survivors.is_empty(),
        "survived the supervisor: {survivors:?}"
    );
    Ok(())
}

/// A session keeps starting services after its binary was removed, as during an upgrade: the
/// shim runs from the image the supervisor was started from.
#[cfg(target_os = "linux")]
#[test]
fn services_restart_after_the_binary_is_removed() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let binary = directory.path().join("micromux-copy");
    std::fs::copy(MICROMUX, &binary)?;
    std::fs::write(
        directory.path().join("micromux.yaml"),
        "version: 1\nservices:\n  svc:\n    command: [\"sh\", \"-c\", \"echo $$ > svc.pid; exec sleep 300\"]\n",
    )?;
    let mut supervisor = serve(&binary, directory.path())?;
    let first = read_pids(directory.path(), &["svc.pid"], &mut supervisor)?;
    std::fs::remove_file(&binary)?;
    std::fs::remove_file(directory.path().join("svc.pid"))?;

    let restarted = Command::new(MICROMUX)
        .args(["ctl", "restart", "svc"])
        .current_dir(directory.path())
        .stdout(Stdio::null())
        .status()?;
    let second = read_pids(directory.path(), &["svc.pid"], &mut supervisor);
    supervisor.kill()?;
    supervisor.wait()?;

    assert!(restarted.success());
    let second = second?;
    assert_ne!(first, second);
    Ok(())
}

/// The command starts with no blocked or ignored stop signals, whatever the shim does to watch
/// its own.
#[cfg(target_os = "linux")]
#[test]
fn shim_starts_commands_with_default_signal_handling() -> eyre::Result<()> {
    let output = shim(
        std::process::id(),
        &["grep", "-E", "^Sig(Blk|Ign):", "/proc/self/status"],
    )
    .output()?;
    let status = String::from_utf8(output.stdout)?;

    let mask = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|value| u64::from_str_radix(value.trim(), 16).ok())
    };
    // Bits are signal number minus one: HUP, INT, QUIT, USR1, USR2, PIPE, TERM, CHLD.
    let stop_signals: u64 = [1, 2, 3, 10, 12, 13, 15, 17]
        .iter()
        .map(|signal| 1u64 << (signal - 1))
        .sum();
    assert_eq!(mask("SigBlk:"), Some(0), "{status}");
    assert_eq!(
        mask("SigIgn:").map(|ignored| ignored & stop_signals),
        Some(0),
        "{status}"
    );
    assert!(output.status.success());
    Ok(())
}

/// The shim exits with its command's status, and reports a command it cannot start like a
/// shell does.
#[test]
fn shim_reports_the_command_status() -> eyre::Result<()> {
    let run = |command: &[&str]| -> eyre::Result<Option<i32>> {
        Ok(shim(std::process::id(), command)
            .stderr(Stdio::null())
            .status()?
            .code())
    };

    assert_eq!(run(&["sh", "-c", "exit 7"])?, Some(7));
    assert_eq!(run(&["definitely-not-a-program-micromux"])?, Some(127));
    // The command does not inherit the variables that carried it.
    assert_eq!(
        run(&["sh", "-c", "[ -z \"$MICROMUX_SHIM_ARG_0\" ] || exit 9"])?,
        Some(0)
    );
    Ok(())
}

/// A supervisor that dies during a graceful stop leaves the stop to the shim without signaling
/// again, since a second interrupt often means "abort the cleanup".
#[test]
fn supervisor_death_during_a_stop_does_not_repeat_the_signal() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("graceful.sh"),
        r"trap 'echo int >> interrupts' INT
echo $$ > svc.pid
while :; do sleep 0.05; done
",
    )?;
    std::fs::write(
        directory.path().join("micromux.yaml"),
        "version: 1\nservices:\n  svc:\n    command: [\"sh\", \"./graceful.sh\"]\n    stop_signal: SIGINT\n    stop_grace_period: 2s\n",
    )?;
    let mut supervisor = serve(Path::new(MICROMUX), directory.path())?;
    let pids = read_pids(directory.path(), &["svc.pid"], &mut supervisor)?;

    // Start a graceful stop, and kill the supervisor once the service saw the interrupt.
    let disabled = Command::new(MICROMUX)
        .args(["ctl", "disable", "svc"])
        .current_dir(directory.path())
        .stdout(Stdio::null())
        .status()?;
    let interrupts = directory.path().join("interrupts");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !interrupts.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    supervisor.kill()?;
    supervisor.wait()?;

    // The shim finishes the stop within the remaining grace period.
    let deadline = Instant::now() + Duration::from_secs(10);
    while pids.iter().copied().any(alive) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let survivors = pids
        .iter()
        .copied()
        .filter(|pid| alive(*pid))
        .collect::<Vec<_>>();
    for pid in &survivors {
        let _ = kill(Pid::from_raw(*pid), Signal::SIGKILL);
    }

    assert!(disabled.success());
    eyre::ensure!(
        survivors.is_empty(),
        "survived the supervisor: {survivors:?}"
    );
    assert_eq!(std::fs::read_to_string(&interrupts)?, "int\n");
    Ok(())
}

/// A command killed by a real-time signal still ends the shim with that signal's status.
#[cfg(target_os = "linux")]
#[test]
fn shim_reports_real_time_signal_deaths() -> eyre::Result<()> {
    use std::os::unix::process::ExitStatusExt as _;

    let mut shim = shim(std::process::id(), &["sh", "-c", "kill -s RTMIN+1 $$"]).spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = shim.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = shim.kill();
            eyre::bail!("the shim kept running after its command died");
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    // Real-time signals have no default-action emulation, so the shim reports them the way a
    // shell does, as 128 plus the signal number.
    let signal = 35;
    assert!(
        status.signal() == Some(signal) || status.code() == Some(128 + signal),
        "{status:?}"
    );
    Ok(())
}

/// Waits for every pid to exit, SIGKILLs the survivors, and returns them.
fn survivors_after(pids: &[i32], timeout: Duration) -> Vec<i32> {
    let deadline = Instant::now() + timeout;
    while pids.iter().copied().any(alive) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let survivors = pids
        .iter()
        .copied()
        .filter(|pid| alive(*pid))
        .collect::<Vec<_>>();
    for pid in &survivors {
        let _ = kill(Pid::from_raw(*pid), Signal::SIGKILL);
    }
    survivors
}

/// A service stopped with `SIGHUP` gets it once from the shim when the supervisor dies, although
/// the terminal's own hangup reaches only the shim.
#[test]
fn sigkilled_supervisor_hangs_up_hup_services_once() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("hup.sh"),
        r"trap 'echo hup >> hangups; exit 0' HUP
echo $$ > svc.pid
while :; do sleep 0.05; done
",
    )?;
    std::fs::write(
        directory.path().join("micromux.yaml"),
        "version: 1\nservices:\n  svc:\n    command: [\"sh\", \"./hup.sh\"]\n    stop_signal: SIGHUP\n    stop_grace_period: 60s\n",
    )?;
    let mut supervisor = serve(Path::new(MICROMUX), directory.path())?;
    let pids = read_pids(directory.path(), &["svc.pid"], &mut supervisor)?;
    supervisor.kill()?;
    supervisor.wait()?;

    // Well within the grace period, so the service ended on the signal, not the SIGKILL.
    let survivors = survivors_after(&pids, Duration::from_secs(20));
    eyre::ensure!(
        survivors.is_empty(),
        "survived the supervisor: {survivors:?}"
    );
    assert_eq!(
        std::fs::read_to_string(directory.path().join("hangups"))?,
        "hup\n"
    );
    Ok(())
}

/// A signal another process sends to the pid Micromux shows for a service reaches the command.
#[test]
fn shim_forwards_signals_from_other_processes() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut shim = shim(
        std::process::id(),
        &[
            "sh",
            "-c",
            "trap 'echo usr1 > got; exit 3' USR1; touch ready; while :; do sleep 0.05; done",
        ],
    )
    .current_dir(directory.path())
    .stdin(Stdio::null())
    // As under the supervisor, the shim leads its group, which the sender is not part of.
    .process_group(0)
    .spawn()?;
    let ready = directory.path().join("ready");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    // `kill(1)` is a sender other than the supervisor, which this test plays.
    let sent = Command::new("kill")
        .args(["-USR1", &shim.id().to_string()])
        .status()?;

    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = shim.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = shim.kill();
            let _ = shim.wait();
            eyre::bail!("the command never saw the forwarded signal");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(sent.success());
    assert_eq!(status.code(), Some(3));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("got"))?,
        "usr1\n"
    );
    Ok(())
}

/// A shim that does not lead its process group cleans up only its own descendants, never the
/// rest of the group it was started in.
#[test]
fn shim_spares_a_process_group_it_does_not_lead() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut canary = Command::new("sleep")
        .arg("60")
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()?;
    let group = i32::try_from(canary.id())?;
    // The shell starts the shim in the background and exits, so the shim outlives its
    // supervisor inside the canary's group.
    let started = Command::new("sh")
        .args([
            "-c",
            r#""$0" __shim --supervisor-pid $$ --grace-ms 200 &
echo $! > shim.pid
while [ ! -s command.pid ]; do sleep 0.02; done"#,
            MICROMUX,
        ])
        .env("MICROMUX_SHIM_ARG_0", "sh")
        .env("MICROMUX_SHIM_ARG_1", "-c")
        .env(
            "MICROMUX_SHIM_ARG_2",
            "echo $$ > command.pid; exec sleep 60",
        )
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .process_group(group)
        .status();
    let deadline = Instant::now() + Duration::from_secs(10);
    let pids = ["shim.pid", "command.pid"]
        .iter()
        .map(|name| read_pid(&directory.path().join(name), deadline))
        .collect::<eyre::Result<Vec<_>>>();
    let survivors = pids
        .as_ref()
        .map(|pids| survivors_after(pids, Duration::from_secs(10)))
        .unwrap_or_default();
    let canary_alive = alive(group);
    let _ = canary.kill();
    let _ = canary.wait();

    assert!(started?.success());
    pids?;
    eyre::ensure!(
        survivors.is_empty(),
        "survived the shim's cleanup: {survivors:?}"
    );
    assert!(canary_alive, "the shim signaled a group it does not lead");
    Ok(())
}

/// A signal sent to the whole process group from inside the service reaches the command once:
/// the shim, a member of that group, does not pass it on again.
#[test]
fn shim_does_not_forward_signals_from_its_own_group() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let status = shim(
        std::process::id(),
        &[
            "sh",
            "-c",
            "trap 'echo usr1 >> got' USR1; kill -USR1 0; sleep 0.5; exit 0",
        ],
    )
    .current_dir(directory.path())
    .stdin(Stdio::null())
    .process_group(0)
    .status()?;

    assert!(status.success(), "{status:?}");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("got"))?,
        "usr1\n"
    );
    Ok(())
}

/// Removes a test session's cleanup files from the user's state directory when dropped.
struct CleanupState(Option<std::path::PathBuf>);

impl CleanupState {
    /// The cleanup state of the session whose config is `micromux.yaml` in `directory`.
    fn of(directory: &Path) -> eyre::Result<Self> {
        let config = directory.join("micromux.yaml").canonicalize()?;
        Ok(Self(micromux::cleanup_state_directory(&config)))
    }

    fn file(&self, name: &str) -> eyre::Result<std::path::PathBuf> {
        Ok(self
            .0
            .as_ref()
            .ok_or_else(|| eyre::eyre!("no state directory"))?
            .join(name))
    }

    fn lock(&self, service: &str) -> eyre::Result<std::path::PathBuf> {
        self.file(&format!("{service}.lock"))
    }

    fn log(&self, service: &str) -> eyre::Result<std::path::PathBuf> {
        self.file(&format!("{service}.log"))
    }
}

impl Drop for CleanupState {
    fn drop(&mut self) {
        if let Some(directory) = &self.0 {
            let _ = std::fs::remove_dir_all(directory);
        }
    }
}

/// Waits until `path` holds at least `lines` lines, and returns its contents.
fn read_lines(path: &Path, lines: usize, timeout: Duration) -> eyre::Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.lines().count() >= lines {
            return Ok(text);
        }
        eyre::ensure!(
            Instant::now() < deadline,
            "{} has {:?} instead of {lines} lines",
            path.display(),
            text
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A supervisor killed with `SIGKILL` while its service runs leaves the cleanup to the service's
/// shim, which runs it exactly once, after the service's processes are gone, in the service's
/// environment and working directory.
#[test]
fn sigkilled_supervisor_runs_the_cleanup_exactly_once() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let work = directory.path().join("work");
    std::fs::create_dir(&work)?;
    std::fs::write(
        directory.path().join("micromux.yaml"),
        r#"version: 1
services:
  svc:
    command: ["sh", "-c", "echo $$ > svc.pid; exec sleep 300"]
    working_dir: work
    environment:
      CLEANUP_MARKER: from-service
    cleanup:
      command: ["sh", "-c", "echo $$CLEANUP_MARKER >> cleanups"]
"#,
    )?;
    let state = CleanupState::of(directory.path())?;

    let mut supervisor = serve(Path::new(MICROMUX), directory.path())?;
    let pids = read_pids(&work, &["svc.pid"], &mut supervisor)?;
    supervisor.kill()?;
    supervisor.wait()?;

    let survivors = survivors_after(&pids, Duration::from_secs(20));
    eyre::ensure!(
        survivors.is_empty(),
        "survived the supervisor: {survivors:?}"
    );
    // The cleanup ran in `work` with the service's variable, and only once even after settling.
    read_lines(&work.join("cleanups"), 1, Duration::from_secs(20))?;
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        std::fs::read_to_string(work.join("cleanups"))?,
        "from-service\n"
    );
    // The shim framed the cleanup's log itself, having no run to name.
    let log = std::fs::read_to_string(state.log("svc")?)?;
    let lines = log.lines().collect::<Vec<_>>();
    assert!(
        lines
            .first()
            .is_some_and(|header| header.contains("after micromux exited")),
        "{log}"
    );
    assert_eq!(lines.last(), Some(&"micromux: cleanup exited with 0"));
    Ok(())
}

/// A service shim that outlives its supervisor holds the cleanup lock from the moment it starts
/// tearing the service down, so a restarted session waits for the old tree as well as for its
/// cleanup.
#[test]
fn shims_hold_the_cleanup_lock_through_the_whole_teardown() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("slow.sh"),
        "trap 'sleep 3; exit 0' TERM HUP\necho $$ > svc.pid\nwhile :; do sleep 0.05; done\n",
    )?;
    std::fs::write(
        directory.path().join("micromux.yaml"),
        r#"version: 1
services:
  svc:
    command: ["sh", "./slow.sh"]
    cleanup:
      command: ["sh", "-c", "echo done >> cleanups"]
"#,
    )?;
    let state = CleanupState::of(directory.path())?;

    let mut supervisor = serve(Path::new(MICROMUX), directory.path())?;
    let pids = read_pids(directory.path(), &["svc.pid"], &mut supervisor)?;
    supervisor.kill()?;
    supervisor.wait()?;

    // While the service still sits in its slow trap, the cleanup has not run, and its lock is
    // already taken.
    let deadline = Instant::now() + Duration::from_secs(2);
    let held = loop {
        let held = std::fs::File::open(state.lock("svc")?)
            .is_ok_and(|lock| matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
        if held || Instant::now() >= deadline {
            break held;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let cleaned_early = directory.path().join("cleanups").exists();
    let service_alive = pids.iter().copied().any(alive);

    read_lines(
        &directory.path().join("cleanups"),
        1,
        Duration::from_secs(20),
    )?;
    let survivors = survivors_after(&pids, Duration::from_secs(10));
    assert!(held, "the cleanup lock was free during the teardown");
    assert!(
        service_alive,
        "the service was gone before the lock was checked"
    );
    assert!(
        !cleaned_early,
        "the cleanup ran before the service was gone"
    );
    eyre::ensure!(
        survivors.is_empty(),
        "survived the supervisor: {survivors:?}"
    );
    Ok(())
}

/// A cleanup the supervisor started finishes, exactly once and within its timeout, when the
/// supervisor is killed while it runs.
#[test]
fn cleanup_outlives_a_supervisor_killed_during_it() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("micromux.yaml"),
        r#"version: 1
services:
  svc:
    command: ["sh", "-c", "echo $$ > svc.pid; exec sleep 300"]
    cleanup:
      command: ["sh", "-c", "echo started >> cleanups; sleep 2; echo done >> cleanups"]
      timeout: 30s
"#,
    )?;
    let state = CleanupState::of(directory.path())?;
    let mut supervisor = serve(Path::new(MICROMUX), directory.path())?;
    let pids = read_pids(directory.path(), &["svc.pid"], &mut supervisor)?;

    // Stopping the service starts its cleanup; the supervisor dies while it runs.
    let disabled = Command::new(MICROMUX)
        .args(["ctl", "disable", "svc"])
        .current_dir(directory.path())
        .stdout(Stdio::null())
        .status()?;
    let started = read_lines(
        &directory.path().join("cleanups"),
        1,
        Duration::from_secs(20),
    );
    supervisor.kill()?;
    supervisor.wait()?;
    started?;

    let cleanups = read_lines(
        &directory.path().join("cleanups"),
        2,
        Duration::from_secs(20),
    )?;
    std::thread::sleep(Duration::from_secs(1));
    assert!(disabled.success());
    assert_eq!(cleanups, "started\ndone\n");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("cleanups"))?,
        "started\ndone\n"
    );
    assert_eq!(
        survivors_after(&pids, Duration::from_secs(1)),
        Vec::<i32>::new()
    );
    // The cleanup's shim exited, releasing the lock that would hold back a restarted session.
    let lock = std::fs::File::open(state.lock("svc")?)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while lock.try_lock().is_err() {
        eyre::ensure!(Instant::now() < deadline, "the cleanup's shim never exited");
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// A cleanup-only shim stops a cleanup that ignores the stop signal past its timeout with
/// `SIGKILL` after the stop grace, kills it, and reports the timeout as status 124.
#[test]
fn cleanup_only_shim_kills_a_cleanup_past_its_timeout() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let log = directory.path().join("cleanup.log");
    let started = Instant::now();
    let mut shim = Command::new(MICROMUX)
        .args([
            "__shim",
            "--cleanup-only",
            "--stop-signal",
            "TERM",
            "--cleanup-timeout-ms",
            "500",
            "--cleanup-log",
        ])
        .arg(&log)
        .env("MICROMUX_SHIM_ARG_0", "sh")
        .env("MICROMUX_SHIM_ARG_1", "-c")
        .env(
            "MICROMUX_SHIM_ARG_2",
            "trap '' TERM; echo $$ > cleanup.pid; exec sleep 60",
        )
        .current_dir(directory.path())
        // A closed stdin starts the cleanup at once, as the death of a supervisor does.
        .stdin(Stdio::null())
        .process_group(0)
        .spawn()?;

    // The timeout, the stop grace, and slack for a loaded machine
    let deadline = started + Duration::from_millis(500) + Duration::from_secs(5 + 5);
    let status = loop {
        if let Some(status) = shim.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = kill(Pid::from_raw(-i32::try_from(shim.id())?), Signal::SIGKILL);
            let _ = shim.wait();
            eyre::bail!("the cleanup-only shim outlived its cleanup's timeout");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let pid = read_pid(
        &directory.path().join("cleanup.pid"),
        Instant::now() + Duration::from_secs(1),
    )?;

    assert_eq!(status.code(), Some(124));
    assert_eq!(
        survivors_after(&[pid], Duration::from_secs(1)),
        Vec::<i32>::new()
    );
    assert_eq!(
        std::fs::read_to_string(&log)?.lines().last(),
        Some("micromux: cleanup timed out after 500ms")
    );
    Ok(())
}

/// A session waits to start its services while an earlier session's cleanup still holds its
/// cleanup lock, even for a service the config no longer has.
#[test]
fn sessions_wait_for_a_previous_sessions_cleanup() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("micromux.yaml"),
        "version: 1\nservices:\n  svc:\n    command: [\"sh\", \"-c\", \"echo $$ > svc.pid; exec sleep 300\"]\n",
    )?;
    let state = CleanupState::of(directory.path())?;
    let lock_path = state.lock("removed")?;
    if let Some(parent) = lock_path.parent() {
        use std::os::unix::fs::DirBuilderExt as _;

        // As private as micromux creates it, since it lives in the user's state directory.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let lock = std::fs::File::create(&lock_path)?;
    lock.lock()?;

    let mut supervisor = serve(Path::new(MICROMUX), directory.path())?;
    std::thread::sleep(Duration::from_secs(2));
    let started_early = directory.path().join("svc.pid").exists();
    drop(lock);
    let pids = read_pids(directory.path(), &["svc.pid"], &mut supervisor);
    supervisor.kill()?;
    supervisor.wait()?;

    let pids = pids?;
    assert!(
        !started_early,
        "the service started while the lock was held"
    );
    let _ = survivors_after(&pids, Duration::from_secs(20));
    Ok(())
}
