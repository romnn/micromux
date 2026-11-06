//! Cleaning up service process trees when the supervisor itself dies.
//!
//! A supervisor killed with SIGKILL runs no cleanup, and its services' processes would live on.
//! When containment is enabled, every service and healthcheck command runs under a small shim,
//! this same binary invoked as `micromux __shim`, which starts the command as its child and
//! watches the supervisor:
//!
//! - It compares its parent with the supervisor's pid before starting the command and every
//!   [`POLL_INTERVAL`] afterwards; a different parent means the supervisor is gone.
//! - Once the supervisor is gone it sends the service's stop signal to every descendant, waits
//!   up to the grace period, then SIGKILLs whatever is left, and exits.
//! - On Linux it is a child subreaper, so descendants that detach into their own session (as
//!   task runners such as turborepo do) are re-parented to it and stay reachable.
//!   macOS has no subreaper: a process orphaned before the supervisor died escapes, so cleanup
//!   there is best effort.
//!
//! While the supervisor lives the shim stays out of the way: it never forwards signals, because
//! the supervisor always signals the whole process group (which includes the command) and
//! sweeps escaped descendants itself, and a terminal-generated signal reaches the foreground
//! group directly.
//! When the command exits on its own, the shim exits with the same status.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// The hidden subcommand the shim runs as.
pub const SHIM_COMMAND: &str = "__shim";

/// How often the shim checks whether the supervisor still lives.
#[cfg(unix)]
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// How long the shim waits after a terminal hangup before trusting that the supervisor lives.
///
/// The kernel closes a dying supervisor's descriptors, which hangs up the services' terminals,
/// before it re-parents the shim, so a command can die of the hangup while the shim still sees
/// the supervisor as its parent.
#[cfg(unix)]
const HANGUP_SETTLE: Duration = Duration::from_millis(500);

/// Exit status of a shim whose command could not be started, as a shell reports it.
#[cfg(unix)]
const EXIT_CANNOT_RUN: i32 = 127;

/// Exit status of a shim invoked with arguments it does not understand.
#[cfg(unix)]
const EXIT_USAGE: i32 = 2;

static SHIM: OnceLock<PathBuf> = OnceLock::new();

/// Runs every service and healthcheck command started from now on under the shim at
/// `executable`, which must run [`run_shim`] when invoked with [`SHIM_COMMAND`].
///
/// Only the first call takes effect.
/// Has no effect on platforms without a shim.
pub fn enable(executable: PathBuf) {
    if cfg!(unix) {
        let _ = SHIM.set(executable);
    }
}

/// The argv that runs `program` with `args` under the shim, or `None` when containment is off.
pub(crate) fn wrap(
    program: &Path,
    args: &[String],
    stop_signal: crate::spec::StopSignal,
    grace: Duration,
) -> Option<(PathBuf, Vec<OsString>)> {
    let shim = SHIM.get()?;
    let mut shim_args: Vec<OsString> = vec![
        SHIM_COMMAND.into(),
        "--supervisor-pid".into(),
        std::process::id().to_string().into(),
        "--stop-signal".into(),
        stop_signal_name(stop_signal).into(),
        "--grace-ms".into(),
        grace.as_millis().to_string().into(),
        "--".into(),
        program.as_os_str().to_owned(),
    ];
    shim_args.extend(args.iter().map(OsString::from));
    Some((shim.clone(), shim_args))
}

fn stop_signal_name(signal: crate::spec::StopSignal) -> &'static str {
    use crate::spec::StopSignal;
    match signal {
        StopSignal::Term => "TERM",
        StopSignal::Int => "INT",
        StopSignal::Hup => "HUP",
        StopSignal::Quit => "QUIT",
        StopSignal::Usr1 => "USR1",
        StopSignal::Usr2 => "USR2",
    }
}

/// Runs the shim with the arguments that follow [`SHIM_COMMAND`] and returns its exit status.
///
/// Call it before starting an async runtime or anything else: the shim is meant to stay a small,
/// single-purpose process.
#[cfg(unix)]
#[must_use]
pub fn run_shim(args: impl Iterator<Item = OsString>) -> i32 {
    match ShimArgs::parse(args) {
        Ok(args) => shim::run(&args),
        Err(message) => {
            eprintln!("micromux shim: {message}");
            EXIT_USAGE
        }
    }
}

/// The shim only exists on Unix.
#[cfg(not(unix))]
#[must_use]
pub fn run_shim(_args: impl Iterator<Item = OsString>) -> i32 {
    eprintln!("micromux shim: not supported on this platform");
    2
}

#[cfg(unix)]
#[derive(Debug)]
struct ShimArgs {
    supervisor: u32,
    stop_signal: nix::sys::signal::Signal,
    grace: Duration,
    program: OsString,
    args: Vec<OsString>,
}

#[cfg(unix)]
impl ShimArgs {
    fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Self, String> {
        let mut supervisor = None;
        let mut stop_signal = nix::sys::signal::Signal::SIGTERM;
        let mut grace = Duration::from_secs(10);
        loop {
            let flag = args
                .next()
                .ok_or_else(|| "missing `-- <program>`".to_string())?;
            let mut value = || {
                args.next()
                    .and_then(|value| value.into_string().ok())
                    .ok_or_else(|| format!("{} needs a value", flag.to_string_lossy()))
            };
            match flag.to_str() {
                Some("--") => break,
                Some("--supervisor-pid") => {
                    supervisor = Some(value()?.parse().map_err(|err| format!("{err}"))?);
                }
                Some("--stop-signal") => {
                    let name = value()?;
                    stop_signal = format!("SIG{name}")
                        .parse()
                        .map_err(|_| format!("unknown signal `{name}`"))?;
                }
                Some("--grace-ms") => {
                    grace =
                        Duration::from_millis(value()?.parse().map_err(|err| format!("{err}"))?);
                }
                _ => return Err(format!("unknown argument `{}`", flag.to_string_lossy())),
            }
        }
        let program = args
            .next()
            .ok_or_else(|| "missing the program after `--`".to_string())?;
        Ok(Self {
            supervisor: supervisor.ok_or_else(|| "missing --supervisor-pid".to_string())?,
            stop_signal,
            grace,
            program,
            args: args.collect(),
        })
    }
}

#[cfg(unix)]
mod shim {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use nix::errno::Errno;
    use nix::sys::signal::Signal;
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    use nix::unistd::Pid;

    use super::{EXIT_CANNOT_RUN, HANGUP_SETTLE, POLL_INTERVAL, ShimArgs};
    use crate::process_tree;

    /// How the main command ended.
    #[derive(Debug, Clone, Copy)]
    enum Ended {
        Exited(i32),
        Signaled(Signal),
    }

    pub(super) fn run(args: &ShimArgs) -> i32 {
        // Adopt every orphan below us, so cleanup can still reach detached descendants.
        #[cfg(target_os = "linux")]
        if let Err(err) = rustix::process::set_child_subreaper(Some(rustix::process::getpid())) {
            eprintln!("micromux shim: cannot become a subreaper: {err}");
        }

        // Handlers, unlike a blocked mask, are reset to the defaults in the command at exec.
        let events = match watch_signals() {
            Ok(events) => events,
            Err(err) => {
                eprintln!("micromux shim: cannot watch signals: {err}");
                return EXIT_CANNOT_RUN;
            }
        };

        if !supervisor_alive(args.supervisor) {
            return 1;
        }
        let child = match std::process::Command::new(&args.program)
            .args(&args.args)
            .spawn()
        {
            Ok(child) => child,
            Err(err) => {
                eprintln!(
                    "micromux: cannot run `{}`: {err}",
                    args.program.to_string_lossy()
                );
                return EXIT_CANNOT_RUN;
            }
        };
        let child = Pid::from_raw(i32::try_from(child.id()).unwrap_or(i32::MAX));

        let mut ended = None;
        let mut hangup_at: Option<Instant> = None;
        loop {
            match events.recv_timeout(POLL_INTERVAL) {
                Ok(Signal::SIGHUP) => hangup_at = Some(Instant::now()),
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                // The watcher thread ended; keep polling at the same pace.
                Err(mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(POLL_INTERVAL),
            }
            reap(child, &mut ended);

            // Supervisor death wins over the command's own exit: its hangup may be what ended
            // the command.
            if !supervisor_alive(args.supervisor) {
                clean_up(args);
                return status(ended);
            }
            if ended.is_some() {
                let settling = hangup_at.is_some_and(|at| at.elapsed() < HANGUP_SETTLE);
                if !settling {
                    return status(ended);
                }
            }
        }
    }

    /// Forwards the signals the shim cares about to a channel, from a thread of its own.
    fn watch_signals() -> std::io::Result<mpsc::Receiver<Signal>> {
        use signal_hook::consts::signal::{
            SIGCHLD, SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2,
        };

        let mut signals = signal_hook::iterator::Signals::new([
            SIGTERM, SIGINT, SIGHUP, SIGQUIT, SIGUSR1, SIGUSR2, SIGCHLD,
        ])?;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for raw in signals.forever() {
                if let Ok(signal) = Signal::try_from(raw)
                    && sender.send(signal).is_err()
                {
                    return;
                }
            }
        });
        Ok(receiver)
    }

    fn supervisor_alive(supervisor: u32) -> bool {
        rustix::process::getppid().is_some_and(|parent| {
            u32::try_from(parent.as_raw_nonzero().get()).ok() == Some(supervisor)
        })
    }

    /// Reaps every exited child, recording the main command's status.
    fn reap(child: Pid, ended: &mut Option<Ended>) {
        loop {
            match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(pid, code)) => {
                    if pid == child {
                        *ended = Some(Ended::Exited(code));
                    }
                }
                Ok(WaitStatus::Signaled(pid, signal, _)) => {
                    if pid == child {
                        *ended = Some(Ended::Signaled(signal));
                    }
                }
                Ok(WaitStatus::StillAlive) | Err(_) => return,
                Ok(_) => {}
            }
        }
    }

    /// Stops every descendant: the stop signal first, then SIGKILL after the grace period, until
    /// none is left.
    fn clean_up(args: &ShimArgs) {
        let Some(own) = process_tree::stamp(std::process::id()) else {
            return;
        };
        let own_group = u32::try_from(rustix::process::getpgrp().as_raw_nonzero().get()).ok();

        // Graceful pass: one signal per process. The group signal reaches the members of the
        // shim's own group, so they are skipped individually; the shim only observes its copy.
        if let Some(group) = own_group.and_then(|group| i32::try_from(group).ok()) {
            let _ = nix::sys::signal::killpg(Pid::from_raw(group), args.stop_signal);
        }
        for descendant in process_tree::descendants(own) {
            if descendant.process_group.is_none() || descendant.process_group != own_group {
                let _ = process_tree::send_signal(descendant.stamp, args.stop_signal);
            }
        }
        let deadline = Instant::now() + args.grace;
        while Instant::now() < deadline && !process_tree::descendants(own).is_empty() {
            reap_all();
            std::thread::sleep(Duration::from_millis(20));
        }

        // Forced pass: one process at a time, never the group, so the shim survives to reap and
        // to catch processes forked in the meantime.
        for _ in 0..1000 {
            let remaining = process_tree::descendants(own);
            if remaining.is_empty() && no_children_left() {
                return;
            }
            for descendant in remaining {
                let _ = process_tree::send_signal(descendant.stamp, Signal::SIGKILL);
            }
            reap_all();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Reaps every exited child; after the supervisor died their statuses no longer matter.
    fn reap_all() {
        while let Ok(status) = waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            if status == WaitStatus::StillAlive {
                return;
            }
        }
    }

    /// Whether the shim has no child left to reap, which on Linux also covers adopted orphans.
    fn no_children_left() -> bool {
        matches!(
            waitpid(None, Some(WaitPidFlag::WNOHANG)),
            Err(Errno::ECHILD)
        )
    }

    /// The shim's own exit status for how the command ended.
    fn status(ended: Option<Ended>) -> i32 {
        match ended {
            Some(Ended::Exited(code)) => code,
            Some(Ended::Signaled(signal)) => {
                // Die of the same signal, so the supervisor sees what the command saw.
                let _ = signal_hook::low_level::emulate_default_handler(signal as i32);
                128 + signal as i32
            }
            None => 1,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use similar_asserts::assert_eq;

    fn os(args: &[&str]) -> impl Iterator<Item = OsString> {
        args.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn shim_arguments_round_trip() -> Result<(), String> {
        let parsed = ShimArgs::parse(os(&[
            "--supervisor-pid",
            "42",
            "--stop-signal",
            "INT",
            "--grace-ms",
            "1500",
            "--",
            "sh",
            "-c",
            "echo hi",
        ]))?;
        assert_eq!(parsed.supervisor, 42);
        assert_eq!(parsed.stop_signal, nix::sys::signal::Signal::SIGINT);
        assert_eq!(parsed.grace, Duration::from_millis(1500));
        assert_eq!(parsed.program, OsString::from("sh"));
        assert_eq!(
            parsed.args,
            vec![OsString::from("-c"), OsString::from("echo hi")]
        );
        Ok(())
    }

    #[test]
    fn shim_arguments_need_a_supervisor_and_a_program() {
        assert!(ShimArgs::parse(os(&["--", "sh"])).is_err());
        assert!(ShimArgs::parse(os(&["--supervisor-pid", "1"])).is_err());
        assert!(ShimArgs::parse(os(&["--supervisor-pid", "1", "--bogus", "--", "sh"])).is_err());
    }
}
