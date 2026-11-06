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

/// A supervisor killed with `SIGKILL` takes its services' whole process trees with it: the
/// service's own process, a child in its process group, and a child that detached into a session
/// of its own.
#[test]
fn sigkilled_supervisor_takes_service_trees_with_it() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("micromux.yaml"),
        r#"version: 1
services:
  tree:
    command: ["sh", "-c", "setsid sh -c 'echo $$ > detached.pid; exec sleep 300' & sh -c 'echo $$ > grouped.pid; exec sleep 300' & echo $$ > leader.pid; wait"]
"#,
    )?;
    // Its own process group keeps the supervisor's signals away from the test runner.
    let mut supervisor = Command::new(MICROMUX)
        .arg("serve")
        .current_dir(directory.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;

    let deadline = Instant::now() + Duration::from_secs(30);
    let pids = ["detached.pid", "grouped.pid", "leader.pid"]
        .into_iter()
        .map(|name| read_pid(&directory.path().join(name), deadline))
        .collect::<eyre::Result<Vec<_>>>();
    let pids = match pids {
        Ok(pids) => pids,
        Err(err) => {
            let _ = supervisor.kill();
            let _ = supervisor.wait();
            return Err(err);
        }
    };

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
