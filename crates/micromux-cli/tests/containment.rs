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
    let output = Command::new(MICROMUX)
        .args([
            "__shim",
            "--supervisor-pid",
            &std::process::id().to_string(),
            "--",
            "grep",
            "-E",
            "^Sig(Blk|Ign):",
            "/proc/self/status",
        ])
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
    let shim = |command: &[&str]| -> eyre::Result<Option<i32>> {
        let supervisor = std::process::id().to_string();
        let mut args = vec!["__shim", "--supervisor-pid", supervisor.as_str(), "--"];
        args.extend_from_slice(command);
        Ok(Command::new(MICROMUX)
            .args(&args)
            .stderr(Stdio::null())
            .status()?
            .code())
    };

    assert_eq!(shim(&["sh", "-c", "exit 7"])?, Some(7));
    assert_eq!(shim(&["definitely-not-a-program-micromux"])?, Some(127));
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

    let mut shim = Command::new(MICROMUX)
        .args([
            "__shim",
            "--supervisor-pid",
            &std::process::id().to_string(),
            "--",
            "sh",
            "-c",
            "kill -s RTMIN+1 $$",
        ])
        .spawn()?;
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
