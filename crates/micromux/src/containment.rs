//! Cleaning up service process trees when the supervisor itself dies.
//!
//! A supervisor killed with SIGKILL runs no cleanup, and its services' processes would live on.
//! When containment is enabled, every service and healthcheck command runs under a small shim,
//! this same binary invoked as `micromux __shim`, which starts the command as its child and
//! watches the supervisor:
//!
//! - It compares its parent with the supervisor's pid before starting the command and every
//!   [`POLL_INTERVAL`] afterwards; a different parent means the supervisor is gone.
//!   When the command ends while its terminal has been hung up, the supervisor is gone or is
//!   tearing the service down: a dying supervisor closes the terminal before the kernel
//!   re-parents the shim, so the hangup can end the command first.
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
//!
//! What stays outside: a process that detaches in the milliseconds between a forced stop's last
//! process walk and its group kill, if the supervisor dies right then, and descendants of a probe
//! that already detached when its timeout kills it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// The hidden subcommand the shim runs as.
pub const SHIM_COMMAND: &str = "__shim";

/// How often the shim checks whether the supervisor still lives.
#[cfg(unix)]
const POLL_INTERVAL: Duration = Duration::from_millis(100);

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

/// How the shim stops a command's processes once the supervisor is gone: the owning service's
/// `stop_signal` and `stop_grace_period`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StopPolicy {
    pub(crate) signal: crate::spec::StopSignal,
    pub(crate) grace: Duration,
}

impl Default for StopPolicy {
    fn default() -> Self {
        Self {
            signal: crate::spec::StopSignal::default(),
            grace: crate::spec::DEFAULT_STOP_GRACE_PERIOD,
        }
    }
}

/// The argv that runs `program` with `args` under the shim, or `None` when containment is off.
pub(crate) fn wrap(
    program: &Path,
    args: &[String],
    StopPolicy {
        signal: stop_signal,
        grace,
    }: StopPolicy,
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

    use nix::sys::signal::Signal;
    use rustix::process::{Pid, WaitOptions, WaitStatus};

    use super::{EXIT_CANNOT_RUN, POLL_INTERVAL, ShimArgs};
    use crate::process_tree::{self, Descendant};

    /// How the main command ended, with the raw signal number so real-time signals survive.
    #[derive(Debug, Clone, Copy)]
    enum Ended {
        Exited(i32),
        Signaled(i32),
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
        let child = i32::try_from(child.id()).ok().and_then(Pid::from_raw);

        let mut ended = None;
        // When the supervisor's own graceful stop reached the shim, so cleanup neither repeats
        // the signal nor restarts the grace period.
        let mut stop_requested: Option<Instant> = None;
        loop {
            match events.recv_timeout(POLL_INTERVAL) {
                Ok(signal) if signal == args.stop_signal => {
                    stop_requested.get_or_insert_with(Instant::now);
                }
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                // The watcher thread ended; keep polling at the same pace.
                Err(mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(POLL_INTERVAL),
            }
            reap(child, &mut ended);

            let supervisor_gone = !supervisor_alive(args.supervisor);
            if supervisor_gone || (ended.is_some() && terminal_hung_up()) {
                clean_up(args, stop_requested);
                return status(ended);
            }
            if ended.is_some() {
                return status(ended);
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

    /// Whether the terminal on standard input has been hung up, which a closed PTY master does.
    fn terminal_hung_up() -> bool {
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
        use std::os::fd::AsFd as _;

        let stdin = std::io::stdin();
        let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::empty())];
        poll(&mut fds, PollTimeout::ZERO).is_ok()
            && fds[0]
                .revents()
                .is_some_and(|events| events.contains(PollFlags::POLLHUP))
    }

    /// Reaps every exited child, recording the main command's status.
    fn reap(child: Option<Pid>, ended: &mut Option<Ended>) {
        while let Ok(Some((pid, status))) = rustix::process::waitpid(None, WaitOptions::NOHANG) {
            if Some(pid) != child {
                continue;
            }
            *ended = exit_of(status).or(*ended);
        }
    }

    fn exit_of(status: WaitStatus) -> Option<Ended> {
        status
            .exit_status()
            .map(Ended::Exited)
            .or_else(|| status.terminating_signal().map(Ended::Signaled))
    }

    /// Stops every descendant: the stop signal (unless the supervisor's graceful stop already
    /// delivered it), then SIGKILL after the grace period, until none is left.
    ///
    /// Every process seen is remembered: a process whose parent exits is re-parented away from
    /// the shim on macOS, where only its recorded identity and group still reach it.
    fn clean_up(args: &ShimArgs, stop_requested: Option<Instant>) {
        let Some(own) = process_tree::stamp(std::process::id()) else {
            return;
        };
        let own_group = u32::try_from(rustix::process::getpgrp().as_raw_nonzero().get()).ok();
        let mut retained: Vec<Descendant> = Vec::new();
        let observe = |retained: &mut Vec<Descendant>| {
            for descendant in process_tree::descendants(own) {
                if !retained.iter().any(|seen| seen.stamp == descendant.stamp) {
                    retained.push(descendant);
                }
            }
        };

        // Graceful pass: one signal per process. The group signal reaches the members of the
        // shim's own group, so they are skipped individually; the shim only observes its copy.
        observe(&mut retained);
        let deadline = if let Some(requested) = stop_requested {
            requested + args.grace
        } else {
            if let Some(group) = own_group.and_then(|group| i32::try_from(group).ok()) {
                let _ =
                    nix::sys::signal::killpg(nix::unistd::Pid::from_raw(group), args.stop_signal);
            }
            for descendant in &retained {
                if descendant.process_group.is_none() || descendant.process_group != own_group {
                    let _ = process_tree::send_signal(descendant.stamp, args.stop_signal);
                }
            }
            Instant::now() + args.grace
        };
        while Instant::now() < deadline {
            observe(&mut retained);
            reap_all();
            if retained
                .iter()
                .all(|descendant| !process_tree::is_alive(descendant.stamp))
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        // Forced pass: one process at a time, never the group, so the shim survives to reap and
        // to catch processes forked in the meantime.
        for _ in 0..1000 {
            observe(&mut retained);
            let stamps = retained
                .iter()
                .map(|descendant| descendant.stamp)
                .collect::<Vec<_>>();
            // The shim's own group is scanned too, for members whose parent already exited; the
            // shim itself is the one member spared.
            let groups = retained
                .iter()
                .filter_map(|descendant| descendant.process_group)
                .chain(own_group)
                .collect();
            let survivors = process_tree::expand_survivors(&stamps, &groups)
                .into_iter()
                .filter(|survivor| survivor.pid != own.pid)
                .collect::<Vec<_>>();
            if survivors.is_empty() && no_children_left() {
                return;
            }
            for survivor in survivors {
                let _ = process_tree::send_signal(survivor, Signal::SIGKILL);
            }
            reap_all();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Reaps every exited child; after the supervisor died their statuses no longer matter.
    fn reap_all() {
        while let Ok(Some(_)) = rustix::process::waitpid(None, WaitOptions::NOHANG) {}
    }

    /// Whether the shim has no child left, which on Linux also covers adopted orphans.
    fn no_children_left() -> bool {
        matches!(
            rustix::process::waitpid(None, WaitOptions::NOHANG),
            Err(rustix::io::Errno::CHILD)
        )
    }

    /// The shim's own exit status for how the command ended.
    fn status(ended: Option<Ended>) -> i32 {
        match ended {
            Some(Ended::Exited(code)) => code,
            Some(Ended::Signaled(signal)) => {
                // Die of the same signal, so the supervisor sees what the command saw.
                let _ = signal_hook::low_level::emulate_default_handler(signal);
                128 + signal
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
