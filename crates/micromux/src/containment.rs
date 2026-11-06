//! Cleaning up service process trees when the supervisor itself dies.
//!
//! A supervisor killed with SIGKILL runs no cleanup, and its services' processes would live on.
//! When containment is enabled, every service and healthcheck command runs under a small shim,
//! this same binary invoked as `micromux __shim`, which starts the command as its child and
//! watches the supervisor:
//!
//! - It compares its parent with the supervisor's pid before starting the command and every
//!   [`POLL_INTERVAL`] afterwards; a different parent means the supervisor is gone.
//!   A hung-up terminal means the same, or that the supervisor is tearing the service down: a
//!   dying supervisor closes the terminal before the kernel re-parents the shim, and the hangup
//!   signal that reaches the shim as session leader starts the cleanup at once.
//! - Once the supervisor is gone it sends the service's stop signal to every descendant, waits
//!   up to the grace period, then SIGKILLs whatever is left, and exits.
//! - On Linux it is a child subreaper, so descendants that detach into their own session (as
//!   task runners such as turborepo do) are re-parented to it and stay reachable.
//!   macOS has no subreaper: a process orphaned before the supervisor died escapes, so cleanup
//!   there is best effort.
//!
//! While the supervisor lives the shim stays out of the way. The supervisor always signals the
//! whole process group, which includes the command, and sweeps escaped descendants itself, and a
//! terminal-generated signal reaches the foreground group directly. Only a signal another
//! process sends to the shim, the pid Micromux shows for the service, is forwarded to the
//! command.
//! When the command exits on its own, the shim exits with the same status.
//!
//! What stays outside: a process that detaches in the milliseconds between a forced stop's last
//! process walk and its group kill, if the supervisor dies right then, and descendants that a
//! probe detached before it ended.

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

/// Prefix of the environment variables that carry the command's argv, one per element, numbered
/// from zero.
///
/// The command stays out of the shim's own argv, so a `pkill -f` pattern that matches the command
/// does not match the shim too, which would forward the same signal to the command a second time.
const COMMAND_VARIABLE_PREFIX: &str = "MICROMUX_SHIM_ARG_";

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

/// How to start a command under the shim: the shim's program and argv, and the environment
/// variables that carry the command.
#[derive(Debug)]
pub(crate) struct Wrapped {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    pub(crate) env: Vec<(String, OsString)>,
}

/// How to run `program` with `args` under the shim, or `None` when containment is off.
///
/// `path` and `cwd` are the command's `PATH` and working directory.
///
/// # Errors
///
/// Returns an error when `program` names no executable, so the command fails to spawn in the
/// supervisor just as it would without the shim.
#[cfg_attr(
    not(unix),
    expect(
        clippy::unnecessary_wraps,
        reason = "only Unix has a shim and the program check that can fail; callers handle both"
    )
)]
pub(crate) fn wrap(
    program: &Path,
    args: &[String],
    StopPolicy {
        signal: stop_signal,
        grace,
    }: StopPolicy,
    path: Option<&std::ffi::OsStr>,
    cwd: &Path,
) -> std::io::Result<Option<Wrapped>> {
    let Some(shim) = SHIM.get() else {
        return Ok(None);
    };
    #[cfg(unix)]
    locate(program, path, cwd)?;
    #[cfg(not(unix))]
    let _ = (path, cwd);
    Ok(Some(under(
        shim,
        program,
        args,
        StopPolicy {
            signal: stop_signal,
            grace,
        },
    )))
}

/// The command line that runs `program` with `args` under the shim at `shim`.
fn under(
    shim: &Path,
    program: &Path,
    args: &[String],
    StopPolicy {
        signal: stop_signal,
        grace,
    }: StopPolicy,
) -> Wrapped {
    let shim_args: Vec<OsString> = vec![
        SHIM_COMMAND.into(),
        "--supervisor-pid".into(),
        std::process::id().to_string().into(),
        "--stop-signal".into(),
        stop_signal_name(stop_signal).into(),
        "--grace-ms".into(),
        grace.as_millis().to_string().into(),
    ];
    let env = std::iter::once(program.as_os_str().to_owned())
        .chain(args.iter().map(OsString::from))
        .enumerate()
        .map(|(index, arg)| (format!("{COMMAND_VARIABLE_PREFIX}{index}"), arg))
        .collect();
    Wrapped {
        program: shim.to_path_buf(),
        args: shim_args,
        env,
    }
}

/// Finds the executable `program` names the way `execvp` does: a path with a directory part
/// relative to `cwd`, a bare name in `path`.
#[cfg(unix)]
fn locate(program: &Path, path: Option<&std::ffi::OsStr>, cwd: &Path) -> std::io::Result<PathBuf> {
    use std::io::{Error, ErrorKind};

    let runnable = |candidate: &Path| {
        !candidate.is_dir() && rustix::fs::access(candidate, rustix::fs::Access::EXEC_OK).is_ok()
    };
    let has_directory_part = program
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty());
    if has_directory_part {
        let candidate = cwd.join(program);
        if runnable(&candidate) {
            return Ok(candidate);
        }
        let kind = if candidate.exists() {
            ErrorKind::PermissionDenied
        } else {
            ErrorKind::NotFound
        };
        return Err(Error::new(
            kind,
            format!("`{}` is not an executable file", candidate.display()),
        ));
    }
    std::env::split_paths(path.unwrap_or_default())
        .map(|directory| cwd.join(directory).join(program))
        .find(|candidate| runnable(candidate))
        .ok_or_else(|| {
            Error::new(
                ErrorKind::NotFound,
                format!("`{}` was not found in PATH", program.display()),
            )
        })
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
    match ShimArgs::parse(args, std::env::vars_os()) {
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
    /// The variables that carried the command, which the command does not inherit.
    carriers: Vec<OsString>,
}

#[cfg(unix)]
impl ShimArgs {
    fn parse(
        args: impl Iterator<Item = OsString>,
        env: impl Iterator<Item = (OsString, OsString)>,
    ) -> Result<Self, String> {
        let mut args = args.peekable();
        let mut supervisor = None;
        let mut stop_signal = nix::sys::signal::Signal::SIGTERM;
        let mut grace = Duration::from_secs(10);
        while let Some(flag) = args.next() {
            let mut value = || {
                args.next()
                    .and_then(|value| value.into_string().ok())
                    .ok_or_else(|| format!("{} needs a value", flag.to_string_lossy()))
            };
            match flag.to_str() {
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

        let mut command = env
            .filter_map(|(name, value)| {
                let index = name
                    .to_str()?
                    .strip_prefix(COMMAND_VARIABLE_PREFIX)?
                    .parse::<usize>()
                    .ok()?;
                Some((index, name, value))
            })
            .collect::<Vec<_>>();
        command.sort_by_key(|(index, _, _)| *index);
        if command
            .iter()
            .enumerate()
            .any(|(position, (index, _, _))| position != *index)
        {
            return Err(format!(
                "{COMMAND_VARIABLE_PREFIX}* is not numbered from zero"
            ));
        }
        let (carriers, mut command): (Vec<_>, Vec<_>) = command
            .into_iter()
            .map(|(_, name, value)| (name, value))
            .unzip();
        if command.is_empty() {
            return Err(format!("missing the program in {COMMAND_VARIABLE_PREFIX}0"));
        }
        let program = command.remove(0);
        Ok(Self {
            supervisor: supervisor.ok_or_else(|| "missing --supervisor-pid".to_string())?,
            stop_signal,
            grace,
            program,
            args: command,
            carriers,
        })
    }
}

#[cfg(unix)]
mod shim {
    use std::io::IsTerminal as _;
    use std::os::fd::{AsFd as _, BorrowedFd};
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

    /// A signal the shim received, and the process that sent it, when the system says.
    struct Received {
        signal: Signal,
        sender: Option<i32>,
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
        // Only a terminal hangs up; healthchecks run without one.
        let terminal = std::io::stdin().is_terminal();
        let mut command = std::process::Command::new(&args.program);
        command.args(&args.args);
        for carrier in &args.carriers {
            command.env_remove(carrier);
        }
        let child = match command.spawn() {
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
        let own_pid = i32::try_from(std::process::id()).ok();
        let hung_up = || terminal && hung_up(std::io::stdin().as_fd());

        let mut ended = None;
        // When the supervisor's own graceful stop reached the shim, so cleanup neither repeats
        // the signal nor restarts the grace period.
        let mut stop_requested: Option<Instant> = None;
        loop {
            match events.recv_timeout(POLL_INTERVAL) {
                Ok(Received { signal, sender }) => {
                    let from_supervisor =
                        sender.is_some_and(|sender| u32::try_from(sender) == Ok(args.supervisor));
                    // The kernel's hangup signal can name the dying supervisor as its sender on
                    // some systems, so a signal that comes with a hangup is never the stop.
                    if from_supervisor && signal == args.stop_signal && !hung_up() {
                        stop_requested.get_or_insert_with(Instant::now);
                    } else if !from_supervisor
                        && sender.is_some_and(|sender| sender != 0 && Some(sender) != own_pid)
                        && !sender.is_some_and(in_own_group)
                        && signal != Signal::SIGCHLD
                        && ended.is_none()
                        && let Some(child) = child
                    {
                        let _ = nix::sys::signal::kill(
                            nix::unistd::Pid::from_raw(child.as_raw_nonzero().get()),
                            signal,
                        );
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                // The watcher thread ended; keep polling at the same pace.
                Err(mpsc::RecvTimeoutError::Disconnected) => std::thread::sleep(POLL_INTERVAL),
            }
            let _ = reap(child, &mut ended);

            if !supervisor_alive(args.supervisor) || hung_up() {
                clean_up(args, stop_requested, child, &mut ended);
                return status(ended);
            }
            if ended.is_some() {
                return status(ended);
            }
        }
    }

    /// Forwards the signals the shim cares about, with their senders, to a channel, from a
    /// thread of its own.
    fn watch_signals() -> std::io::Result<mpsc::Receiver<Received>> {
        use signal_hook::consts::signal::{
            SIGCHLD, SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2,
        };
        use signal_hook::iterator::SignalsInfo;
        use signal_hook::iterator::exfiltrator::WithOrigin;

        let mut signals = SignalsInfo::<WithOrigin>::new([
            SIGTERM, SIGINT, SIGHUP, SIGQUIT, SIGUSR1, SIGUSR2, SIGCHLD,
        ])?;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for origin in signals.forever() {
                let Ok(signal) = Signal::try_from(origin.signal) else {
                    continue;
                };
                let event = Received {
                    signal,
                    sender: origin.process.map(|process| process.pid),
                };
                if sender.send(event).is_err() {
                    return;
                }
            }
        });
        Ok(receiver)
    }

    /// Whether `sender` shares the shim's process group, whose signals, such as a `kill 0` from
    /// inside the service, reach the command directly.
    fn in_own_group(sender: i32) -> bool {
        Pid::from_raw(sender).is_some_and(|sender| {
            rustix::process::getpgid(Some(sender)).ok() == Some(rustix::process::getpgrp())
        })
    }

    fn supervisor_alive(supervisor: u32) -> bool {
        rustix::process::getppid().is_some_and(|parent| {
            u32::try_from(parent.as_raw_nonzero().get()).ok() == Some(supervisor)
        })
    }

    /// Whether the terminal `fd` has been hung up, which closing a PTY's master does.
    ///
    /// The hangup is asked for explicitly: macOS reports only the events a poll requests.
    pub(super) fn hung_up(fd: BorrowedFd<'_>) -> bool {
        use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

        let mut fds = [PollFd::new(fd, PollFlags::POLLHUP)];
        poll(&mut fds, PollTimeout::ZERO).is_ok()
            && fds[0]
                .revents()
                .is_some_and(|events| events.contains(PollFlags::POLLHUP))
    }

    /// Reaps every exited child, recording the main command's status, and returns whether any
    /// child is left, which on Linux also covers adopted orphans.
    fn reap(child: Option<Pid>, ended: &mut Option<Ended>) -> bool {
        loop {
            match rustix::process::waitpid(None, WaitOptions::NOHANG) {
                Ok(Some((pid, status))) => {
                    if Some(pid) == child {
                        *ended = exit_of(status).or(*ended);
                    }
                }
                Err(rustix::io::Errno::CHILD) => return false,
                Ok(None) | Err(_) => return true,
            }
        }
    }

    fn exit_of(status: WaitStatus) -> Option<Ended> {
        status
            .exit_status()
            .map(Ended::Exited)
            .or_else(|| status.terminating_signal().map(Ended::Signaled))
    }

    /// Stops every descendant: the stop signal (unless the supervisor's graceful stop already
    /// delivered it within the grace period), then SIGKILL after the grace period, until none is
    /// left.
    ///
    /// Every process seen is remembered: a process whose parent exits is re-parented away from
    /// the shim on macOS, where only its recorded identity and group still reach it.
    fn clean_up(
        args: &ShimArgs,
        stop_requested: Option<Instant>,
        child: Option<Pid>,
        ended: &mut Option<Ended>,
    ) {
        let Some(own) = process_tree::stamp(std::process::id()) else {
            return;
        };
        // The shim's group is the command's only when the shim leads it, as the session leader
        // of a PTY or a probe in a group of its own does; otherwise it is the caller's.
        let own_group = (rustix::process::getpgrp() == rustix::process::getpid())
            .then(|| u32::try_from(rustix::process::getpgrp().as_raw_nonzero().get()).ok())
            .flatten();
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
        // A stop older than the grace period was not the supervisor's last word, as an
        // interrupt typed into the terminal is not.
        observe(&mut retained);
        let deadline = match stop_requested {
            Some(requested) if requested.elapsed() < args.grace => requested + args.grace,
            _ => {
                if let Some(group) = own_group.and_then(|group| i32::try_from(group).ok()) {
                    let _ = nix::sys::signal::killpg(
                        nix::unistd::Pid::from_raw(group),
                        args.stop_signal,
                    );
                }
                for descendant in &retained {
                    if descendant.process_group.is_none() || descendant.process_group != own_group {
                        let _ = process_tree::send_signal(descendant.stamp, args.stop_signal);
                    }
                }
                Instant::now() + args.grace
            }
        };
        while Instant::now() < deadline {
            observe(&mut retained);
            let _ = reap(child, ended);
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
            if survivors.is_empty() && !reap(child, ended) {
                return;
            }
            for survivor in survivors {
                let _ = process_tree::send_signal(survivor, Signal::SIGKILL);
            }
            let _ = reap(child, ended);
            std::thread::sleep(Duration::from_millis(10));
        }
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

    fn env(command: &[&str]) -> impl Iterator<Item = (OsString, OsString)> {
        command
            .iter()
            .enumerate()
            .map(|(index, arg)| {
                (
                    OsString::from(format!("{COMMAND_VARIABLE_PREFIX}{index}")),
                    OsString::from(arg),
                )
            })
            .chain([(OsString::from("HOME"), OsString::from("/home"))])
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
    }

    #[test]
    fn shim_arguments_round_trip() -> Result<(), String> {
        let command = [
            "sh", "-c", "echo hi", "a", "b", "c", "d", "e", "f", "g", "h", "i",
        ];
        let parsed = ShimArgs::parse(
            os(&[
                "--supervisor-pid",
                "42",
                "--stop-signal",
                "INT",
                "--grace-ms",
                "1500",
            ]),
            env(&command),
        )?;
        assert_eq!(parsed.supervisor, 42);
        assert_eq!(parsed.stop_signal, nix::sys::signal::Signal::SIGINT);
        assert_eq!(parsed.grace, Duration::from_millis(1500));
        assert_eq!(parsed.program, OsString::from("sh"));
        assert_eq!(
            parsed.args,
            command[1..].iter().map(OsString::from).collect::<Vec<_>>()
        );
        assert_eq!(parsed.carriers.len(), command.len());
        Ok(())
    }

    #[test]
    fn wrapped_commands_stay_out_of_the_shim_argv() -> Result<(), String> {
        let wrapped = under(
            Path::new("/bin/micromux"),
            Path::new("sh"),
            &["-c".to_string(), "next dev".to_string()],
            StopPolicy::default(),
        );
        assert!(
            !wrapped
                .args
                .iter()
                .any(|arg| arg.to_string_lossy().contains("next")),
            "{wrapped:?}"
        );
        let parsed = ShimArgs::parse(
            wrapped.args.into_iter().skip(1),
            wrapped
                .env
                .into_iter()
                .map(|(name, value)| (OsString::from(name), value)),
        )?;
        assert_eq!(parsed.program, OsString::from("sh"));
        assert_eq!(
            parsed.args,
            vec![OsString::from("-c"), OsString::from("next dev")]
        );
        Ok(())
    }

    #[test]
    fn locate_finds_programs_like_execvp() -> std::io::Result<()> {
        use std::io::ErrorKind;
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let bin = directory.path().join("bin");
        std::fs::create_dir(&bin)?;
        let tool = bin.join("tool");
        std::fs::write(&tool, "#!/bin/sh\n")?;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))?;
        let data = bin.join("data");
        std::fs::write(&data, "")?;
        let path = std::ffi::OsString::from("/nonexistent:bin");
        let locate = |program: &str| locate(Path::new(program), Some(&path), directory.path());

        assert_eq!(locate("tool")?, directory.path().join("bin").join("tool"));
        assert_eq!(locate("bin/tool")?, directory.path().join("bin/tool"));
        assert_eq!(locate(&tool.to_string_lossy())?, tool);
        assert_eq!(
            locate("missing").map_err(|err| err.kind()),
            Err(ErrorKind::NotFound)
        );
        assert_eq!(
            locate("data").map_err(|err| err.kind()),
            Err(ErrorKind::NotFound)
        );
        assert_eq!(
            locate("bin/data").map_err(|err| err.kind()),
            Err(ErrorKind::PermissionDenied)
        );
        assert_eq!(
            locate("bin").map_err(|err| err.kind()),
            Err(ErrorKind::NotFound)
        );
        Ok(())
    }

    #[test]
    fn closing_the_master_hangs_up_the_terminal() -> nix::Result<()> {
        use std::os::fd::AsFd as _;

        let pty = nix::pty::openpty(None, None)?;
        assert!(!shim::hung_up(pty.slave.as_fd()));
        drop(pty.master);
        assert!(shim::hung_up(pty.slave.as_fd()));
        Ok(())
    }

    #[test]
    fn shim_arguments_need_a_supervisor_and_a_program() {
        assert!(ShimArgs::parse(os(&[]), env(&["sh"])).is_err());
        assert!(ShimArgs::parse(os(&["--supervisor-pid", "1"]), env(&[])).is_err());
        assert!(ShimArgs::parse(os(&["--supervisor-pid", "1", "--bogus"]), env(&["sh"])).is_err());
        // A gap in the numbering would silently drop arguments.
        let gapped = [
            (
                OsString::from(format!("{COMMAND_VARIABLE_PREFIX}0")),
                OsString::from("sh"),
            ),
            (
                OsString::from(format!("{COMMAND_VARIABLE_PREFIX}2")),
                OsString::from("x"),
            ),
        ];
        assert!(ShimArgs::parse(os(&["--supervisor-pid", "1"]), gapped.into_iter()).is_err());
    }
}
