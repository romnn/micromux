//! Cleaning up service process trees when the supervisor itself dies.
//!
//! A supervisor killed with SIGKILL runs no cleanup, and its services' processes would live on.
//! When containment is enabled, every service and healthcheck command runs under a small shim,
//! this same binary invoked as `micromux __shim`, which starts the command as its child and
//! watches the supervisor:
//!
//! - It compares its parent with the supervisor's pid before starting the command and every
//!   `POLL_INTERVAL` afterwards; a different parent means the supervisor is gone.
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
//! A service with a `cleanup` command also hands that command to its shim, which runs it once
//! it has stopped the service's processes after the supervisor died.
//! While the supervisor lives, the supervisor runs every cleanup through a second, cleanup-only
//! shim (`--cleanup-only`).
//! That shim starts as soon as a run's process exited, waits for the supervisor's go on its
//! stdin, and finishes the cleanup even if the supervisor dies meanwhile, since a closed stdin
//! means go as well.
//! Either shim holds the cleanup lock while it cleans up, so a restarted session can wait for it
//! (see [`crate::wait_for_previous_cleanups`]).
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

/// How long a shim whose terminal hung up waits for its supervisor to exit before it concludes
/// that a live supervisor hung up the terminal to stop the service.
#[cfg(unix)]
const SUPERVISOR_EXIT_WAIT: Duration = Duration::from_secs(2);

/// How often a cleanup's shim checks whether the cleanup exited.
#[cfg(unix)]
const CLEANUP_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Exit status of a shim whose command could not be started, as a shell reports it.
#[cfg(unix)]
const EXIT_CANNOT_RUN: i32 = 127;

/// Prefix of the environment variables that carry the command's argv, one per element, numbered
/// from zero.
///
/// The command stays out of the shim's own argv, so a `pkill -f` pattern that matches the command
/// does not match the shim too, which would forward the same signal to the command a second time.
const COMMAND_VARIABLE_PREFIX: &str = "MICROMUX_SHIM_ARG_";

/// Prefix of the environment variables that carry a service's cleanup command to its shim, like
/// [`COMMAND_VARIABLE_PREFIX`] does the command itself.
const CLEANUP_VARIABLE_PREFIX: &str = "MICROMUX_SHIM_CLEANUP_";

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
/// `cleanup` is the service's cleanup, which the shim runs itself should the supervisor die;
/// probes have none.
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
    stop: StopPolicy,
    cleanup: Option<&crate::spec::CleanupSpec>,
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
    Ok(Some(under(shim, program, args, stop, cleanup)))
}

/// How to run `cleanup` under a cleanup-only shim, or `None` when containment is off.
///
/// The shim starts the cleanup once it reads a byte from its stdin or the stdin closes, and
/// enforces the cleanup's timeout itself.
pub(crate) fn wrap_cleanup(
    cleanup: &crate::spec::CleanupSpec,
    stop_signal: crate::spec::StopSignal,
) -> Option<Wrapped> {
    SHIM.get()
        .map(|shim| cleanup_under(shim, cleanup, stop_signal))
}

/// The command line that runs `cleanup` under a cleanup-only shim at `shim`.
fn cleanup_under(
    shim: &Path,
    cleanup: &crate::spec::CleanupSpec,
    stop_signal: crate::spec::StopSignal,
) -> Wrapped {
    let args = [
        SHIM_COMMAND,
        "--cleanup-only",
        "--stop-signal",
        stop_signal_name(stop_signal),
    ]
    .into_iter()
    .map(OsString::from)
    .chain(cleanup_flags(cleanup))
    .collect();
    Wrapped {
        program: shim.to_path_buf(),
        args,
        env: carriers(
            COMMAND_VARIABLE_PREFIX,
            cleanup.command.iter().map(OsString::from),
        ),
    }
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
    cleanup: Option<&crate::spec::CleanupSpec>,
) -> Wrapped {
    let mut shim_args: Vec<OsString> = vec![
        SHIM_COMMAND.into(),
        "--supervisor-pid".into(),
        std::process::id().to_string().into(),
        "--stop-signal".into(),
        stop_signal_name(stop_signal).into(),
        "--grace-ms".into(),
        grace.as_millis().to_string().into(),
    ];
    let mut env = carriers(
        COMMAND_VARIABLE_PREFIX,
        std::iter::once(program.as_os_str().to_owned()).chain(args.iter().map(OsString::from)),
    );
    if let Some(cleanup) = cleanup {
        shim_args.extend(cleanup_flags(cleanup));
        env.extend(carriers(
            CLEANUP_VARIABLE_PREFIX,
            cleanup.command.iter().map(OsString::from),
        ));
    }
    Wrapped {
        program: shim.to_path_buf(),
        args: shim_args,
        env,
    }
}

/// The shim flags that describe how to run `cleanup`, apart from its command.
fn cleanup_flags(cleanup: &crate::spec::CleanupSpec) -> Vec<OsString> {
    let mut flags: Vec<OsString> = vec![
        "--cleanup-timeout-ms".into(),
        cleanup.timeout.as_millis().to_string().into(),
    ];
    if let Some(files) = &cleanup.files {
        flags.extend([
            "--cleanup-log".into(),
            files.log.clone().into_os_string(),
            "--cleanup-lock".into(),
            files.lock.clone().into_os_string(),
        ]);
    }
    flags
}

/// The variables that carry `argv`, one per element, named `prefix` plus its position.
fn carriers(prefix: &str, argv: impl Iterator<Item = OsString>) -> Vec<(String, OsString)> {
    argv.enumerate()
        .map(|(index, arg)| (format!("{prefix}{index}"), arg))
        .collect()
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
    match Invocation::parse(args, std::env::vars_os()) {
        Ok(Invocation::Supervise(args)) => shim::run(&args),
        Ok(Invocation::CleanupOnly(args)) => shim::run_cleanup_only(&args),
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

/// What a shim invocation asks for.
#[cfg(unix)]
#[derive(Debug)]
enum Invocation {
    /// Run a service or probe command and clean up after it should the supervisor die.
    Supervise(ShimArgs),
    /// Run a service's cleanup after one of its processes exited.
    CleanupOnly(CleanupOnlyArgs),
}

#[cfg(unix)]
#[derive(Debug)]
struct ShimArgs {
    supervisor: u32,
    stop_signal: nix::sys::signal::Signal,
    grace: Duration,
    program: OsString,
    args: Vec<OsString>,
    /// The service's cleanup, which the shim runs itself once the supervisor is gone.
    cleanup: Option<Cleanup>,
    /// The variables that carried the commands, which neither command inherits.
    carriers: Vec<OsString>,
}

#[cfg(unix)]
#[derive(Debug)]
struct CleanupOnlyArgs {
    stop_signal: nix::sys::signal::Signal,
    cleanup: Cleanup,
    /// The variables that carried the command, which the command does not inherit.
    carriers: Vec<OsString>,
}

/// A cleanup command and how to run it.
#[cfg(unix)]
#[derive(Debug)]
struct Cleanup {
    /// The program and its arguments; never empty.
    command: Vec<OsString>,
    timeout: Duration,
    log: Option<PathBuf>,
    lock: Option<PathBuf>,
}

#[cfg(unix)]
impl Invocation {
    fn parse(
        mut args: impl Iterator<Item = OsString>,
        env: impl Iterator<Item = (OsString, OsString)>,
    ) -> Result<Self, String> {
        let mut supervisor = None;
        let mut stop_signal = nix::sys::signal::Signal::SIGTERM;
        let mut grace = Duration::from_secs(10);
        let mut cleanup_only = false;
        let mut cleanup_timeout = None;
        let mut cleanup_log = None;
        let mut cleanup_lock = None;
        while let Some(flag) = args.next() {
            let name = flag.to_string_lossy().into_owned();
            match name.as_str() {
                "--supervisor-pid" => supervisor = Some(number(&mut args, &name)?),
                "--stop-signal" => {
                    let signal = text(&mut args, &name)?;
                    stop_signal = format!("SIG{signal}")
                        .parse()
                        .map_err(|_| format!("unknown signal `{signal}`"))?;
                }
                "--grace-ms" => grace = Duration::from_millis(number(&mut args, &name)?),
                "--cleanup-only" => cleanup_only = true,
                "--cleanup-timeout-ms" => {
                    cleanup_timeout = Some(Duration::from_millis(number(&mut args, &name)?));
                }
                "--cleanup-log" => cleanup_log = Some(PathBuf::from(value(&mut args, &name)?)),
                "--cleanup-lock" => cleanup_lock = Some(PathBuf::from(value(&mut args, &name)?)),
                _ => return Err(format!("unknown argument `{name}`")),
            }
        }

        let env = env.collect::<Vec<_>>();
        let (mut command, mut carriers) = numbered(&env, COMMAND_VARIABLE_PREFIX)?;
        let (cleanup_command, cleanup_carriers) = numbered(&env, CLEANUP_VARIABLE_PREFIX)?;
        carriers.extend(cleanup_carriers);
        if command.is_empty() {
            return Err(format!("missing the program in {COMMAND_VARIABLE_PREFIX}0"));
        }
        let cleanup = |command| {
            Ok::<_, String>(Cleanup {
                command,
                timeout: cleanup_timeout
                    .ok_or_else(|| "missing --cleanup-timeout-ms".to_string())?,
                log: cleanup_log.clone(),
                lock: cleanup_lock.clone(),
            })
        };
        if cleanup_only {
            return Ok(Self::CleanupOnly(CleanupOnlyArgs {
                stop_signal,
                cleanup: cleanup(command)?,
                carriers,
            }));
        }
        let program = command.remove(0);
        Ok(Self::Supervise(ShimArgs {
            supervisor: supervisor.ok_or_else(|| "missing --supervisor-pid".to_string())?,
            stop_signal,
            grace,
            program,
            args: command,
            cleanup: (!cleanup_command.is_empty())
                .then(|| cleanup(cleanup_command))
                .transpose()?,
            carriers,
        }))
    }
}

#[cfg(unix)]
fn value(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<OsString, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

#[cfg(unix)]
fn text(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<String, String> {
    value(args, flag)?
        .into_string()
        .map_err(|_| format!("{flag} needs a UTF-8 value"))
}

#[cfg(unix)]
fn number<T: std::str::FromStr<Err: std::fmt::Display>>(
    args: &mut impl Iterator<Item = OsString>,
    flag: &str,
) -> Result<T, String> {
    text(args, flag)?
        .parse()
        .map_err(|err| format!("{flag}: {err}"))
}

/// The values of the variables named `prefix` plus a number, in order, and the variables' names.
#[cfg(unix)]
fn numbered(
    env: &[(OsString, OsString)],
    prefix: &str,
) -> Result<(Vec<OsString>, Vec<OsString>), String> {
    let mut entries = env
        .iter()
        .filter_map(|(name, value)| {
            let index = name.to_str()?.strip_prefix(prefix)?.parse::<usize>().ok()?;
            Some((index, name.clone(), value.clone()))
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|(index, _, _)| *index);
    // A gap in the numbering would silently drop arguments.
    if entries
        .iter()
        .enumerate()
        .any(|(position, (index, _, _))| position != *index)
    {
        return Err(format!("{prefix}* is not numbered from zero"));
    }
    Ok(entries
        .into_iter()
        .map(|(_, name, value)| (value, name))
        .unzip())
}

#[cfg(unix)]
mod shim {
    use std::ffi::OsString;
    use std::fs::File;
    use std::io::{IsTerminal as _, Read as _, Write as _};
    use std::os::fd::{AsFd as _, BorrowedFd};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use nix::sys::signal::Signal;
    use rustix::process::{Pid, WaitOptions, WaitStatus};

    use super::{
        CLEANUP_POLL_INTERVAL, Cleanup, CleanupOnlyArgs, EXIT_CANNOT_RUN, POLL_INTERVAL,
        SUPERVISOR_EXIT_WAIT, ShimArgs,
    };
    use crate::cleanup::{CLEANUP_STOP_GRACE, EXIT_TIMED_OUT, Ending};
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

            let supervisor_gone = !supervisor_alive(args.supervisor);
            if supervisor_gone || hung_up() {
                // Claim the cleanup's lock before the teardown, so a restarted session waits for
                // all of it, not just for the cleanup command.
                let lock = args.cleanup.as_ref().and_then(|cleanup| {
                    crate::cleanup::try_lock(cleanup.lock.as_deref()?, args.grace + cleanup.timeout)
                });
                clean_up(
                    args.stop_signal,
                    args.grace,
                    stop_requested,
                    child,
                    &mut ended,
                );
                // A live supervisor that hung up the terminal runs the cleanup itself once it
                // reaps this shim, and exiting releases the lock before it can.
                if let Some(cleanup) = &args.cleanup
                    && (supervisor_gone || supervisor_exits(args.supervisor))
                {
                    let _ = run_cleanup(cleanup, args.stop_signal, &args.carriers, true, lock);
                }
                return status(ended);
            }
            if ended.is_some() {
                return status(ended);
            }
        }
    }

    /// Runs a cleanup once the supervisor says go on stdin, or once the stdin closes because the
    /// supervisor died, and returns the cleanup's status.
    ///
    /// The supervisor already started the log with a header for the run.
    pub(super) fn run_cleanup_only(args: &CleanupOnlyArgs) -> i32 {
        // Adopt the cleanup's orphans, so a timeout still reaches what it detached.
        #[cfg(target_os = "linux")]
        if let Err(err) = rustix::process::set_child_subreaper(Some(rustix::process::getpid())) {
            eprintln!("micromux shim: cannot become a subreaper: {err}");
        }
        // Absorbs the stop signal that stopping the cleanup sends to the shim's own group.
        let signals = match watch_signals() {
            Ok(signals) => signals,
            Err(err) => {
                eprintln!("micromux shim: cannot watch signals: {err}");
                return EXIT_CANNOT_RUN;
            }
        };
        let lock = lock_or_report(&args.cleanup);
        let mut go = [0u8; 1];
        while let Err(err) = std::io::stdin().read(&mut go) {
            if err.kind() != std::io::ErrorKind::Interrupted {
                break;
            }
        }
        // The cleanup's timeout runs from now, which can be long after the lock was taken.
        if let Some(lock) = &lock {
            crate::cleanup::record_deadline(lock, args.cleanup.timeout);
        }
        let status = run_cleanup(&args.cleanup, args.stop_signal, &args.carriers, false, lock);
        drop(signals);
        status
    }

    /// Whether the supervisor exits within [`SUPERVISOR_EXIT_WAIT`].
    ///
    /// A dying supervisor hangs up the terminal before the kernel re-parents the shim, while a
    /// live one hangs it up only to stop a service that ignores its signals.
    fn supervisor_exits(supervisor: u32) -> bool {
        let deadline = Instant::now() + SUPERVISOR_EXIT_WAIT;
        loop {
            if !supervisor_alive(supervisor) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    /// Takes the lock of `cleanup`, if it has one, waiting for its holder, or reports why it
    /// cannot.
    fn lock_or_report(cleanup: &Cleanup) -> Option<File> {
        let path = cleanup.lock.as_deref()?;
        crate::cleanup::lock(path, cleanup.timeout)
            .inspect_err(|err| {
                eprintln!(
                    "micromux shim: cannot lock {}: {err}; running the cleanup unlocked",
                    path.display()
                );
            })
            .ok()
    }

    /// Runs `cleanup` to completion in the shim's own process group and returns its status, 124
    /// once it ran past its timeout.
    ///
    /// Its output goes to the cleanup log, which `fresh` truncates and opens with a header, and
    /// which gets a footer saying how it ended.
    /// `lock` is the cleanup lock when the caller already holds it; otherwise it is taken here.
    /// Whatever the cleanup leaves behind is stopped with it, and a cleanup past its timeout gets
    /// the stop signal and [`CLEANUP_STOP_GRACE`] before it is killed.
    ///
    /// The caller has watched its signals, so the stop signal sent to the shim's own group does
    /// not end the shim, and on Linux it is a subreaper, so detached processes stay reachable.
    fn run_cleanup(
        cleanup: &Cleanup,
        stop_signal: Signal,
        carriers: &[OsString],
        fresh: bool,
        lock: Option<File>,
    ) -> i32 {
        let lock = lock.or_else(|| lock_or_report(cleanup));
        let mut log = cleanup.log.as_deref().and_then(|path| {
            crate::cleanup::open_log(path, fresh)
                .inspect_err(|err| {
                    eprintln!("micromux shim: cannot open {}: {err}", path.display());
                })
                .ok()
        });
        if fresh && let Some(log) = &mut log {
            let _ = writeln!(log, "{}", crate::cleanup::header(None, &cleanup.command));
        }
        let output = |log: &Option<File>| {
            log.as_ref()
                .and_then(|log| log.try_clone().ok())
                .map_or_else(Stdio::null, Stdio::from)
        };

        let spawned = match cleanup.command.split_first() {
            Some((program, args)) => {
                let mut command = std::process::Command::new(program);
                command
                    .args(args)
                    .stdin(Stdio::null())
                    .stdout(output(&log))
                    .stderr(output(&log));
                for carrier in carriers {
                    command.env_remove(carrier);
                }
                command
                    .spawn()
                    .map_err(|err| Ending::CannotRun(err.to_string()))
            }
            None => Err(Ending::CannotRun("the command is empty".to_string())),
        };
        let (ending, status) = match spawned {
            Ok(child) => wait_for_cleanup(
                i32::try_from(child.id()).ok().and_then(Pid::from_raw),
                cleanup.timeout,
                stop_signal,
            ),
            Err(ending) => (ending, EXIT_CANNOT_RUN),
        };
        if let Some(log) = &mut log {
            let _ = writeln!(log, "{}", crate::cleanup::footer(&ending));
        }
        drop(lock);
        status
    }

    /// Waits for the cleanup `child` up to `timeout`, stops what it leaves behind, and returns how
    /// it ended with the shim's exit status for it.
    fn wait_for_cleanup(
        child: Option<Pid>,
        timeout: Duration,
        stop_signal: Signal,
    ) -> (Ending, i32) {
        let deadline = Instant::now() + timeout;
        let mut ended = None;
        let timed_out = loop {
            let _ = reap(child, &mut ended);
            if ended.is_some() {
                break false;
            }
            if Instant::now() >= deadline {
                break true;
            }
            std::thread::sleep(CLEANUP_POLL_INTERVAL);
        };
        // Stop all of a cleanup past its timeout, and whatever a finished one left behind.
        let grace = if timed_out {
            CLEANUP_STOP_GRACE
        } else {
            Duration::ZERO
        };
        clean_up(stop_signal, grace, None, child, &mut ended);

        match ended {
            _ if timed_out => (Ending::TimedOut(timeout), EXIT_TIMED_OUT),
            Some(Ended::Exited(code)) => (Ending::Exited(code), code),
            Some(Ended::Signaled(signal)) => {
                let name = Signal::try_from(signal).map_or_else(
                    |_| format!("signal {signal}"),
                    |signal| signal.as_str().to_string(),
                );
                (Ending::Signaled(name), 128 + signal)
            }
            None => (
                Ending::CannotRun("it could not be waited for".to_string()),
                1,
            ),
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

    /// Stops every descendant: `stop_signal` (unless the supervisor's graceful stop already
    /// delivered it within `grace`), then SIGKILL after `grace`, until none is left.
    ///
    /// Every process seen is remembered: a process whose parent exits is re-parented away from
    /// the shim on macOS, where only its recorded identity and group still reach it.
    ///
    /// The caller has watched its signals: a shim that leads its process group signals that group,
    /// itself included.
    fn clean_up(
        stop_signal: Signal,
        grace: Duration,
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
            Some(requested) if requested.elapsed() < grace => requested + grace,
            _ => {
                if let Some(group) = own_group.and_then(|group| i32::try_from(group).ok()) {
                    let _ =
                        nix::sys::signal::killpg(nix::unistd::Pid::from_raw(group), stop_signal);
                }
                for descendant in &retained {
                    if descendant.process_group.is_none() || descendant.process_group != own_group {
                        let _ = process_tree::send_signal(descendant.stamp, stop_signal);
                    }
                }
                Instant::now() + grace
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

    fn supervise(
        args: impl Iterator<Item = OsString>,
        env: impl Iterator<Item = (OsString, OsString)>,
    ) -> Result<ShimArgs, String> {
        match Invocation::parse(args, env)? {
            Invocation::Supervise(args) => Ok(args),
            Invocation::CleanupOnly(args) => Err(format!("parsed as cleanup-only: {args:?}")),
        }
    }

    fn cleanup_only(
        args: impl Iterator<Item = OsString>,
        env: impl Iterator<Item = (OsString, OsString)>,
    ) -> Result<CleanupOnlyArgs, String> {
        match Invocation::parse(args, env)? {
            Invocation::CleanupOnly(args) => Ok(args),
            Invocation::Supervise(args) => Err(format!("parsed as supervising: {args:?}")),
        }
    }

    fn wrapped_env(wrapped: Wrapped) -> impl Iterator<Item = (OsString, OsString)> {
        wrapped
            .env
            .into_iter()
            .map(|(name, value)| (OsString::from(name), value))
    }

    fn cleanup_spec(files: bool) -> crate::spec::CleanupSpec {
        crate::spec::CleanupSpec {
            command: vec![
                "docker".to_string(),
                "compose".to_string(),
                "down".to_string(),
            ],
            timeout: Duration::from_secs(120),
            files: files.then(|| crate::spec::CleanupFiles {
                log: PathBuf::from("/state/cleanup/key/infra.log"),
                lock: PathBuf::from("/state/cleanup/key/infra.lock"),
            }),
        }
    }

    #[test]
    fn shim_arguments_round_trip() -> Result<(), String> {
        let command = [
            "sh", "-c", "echo hi", "a", "b", "c", "d", "e", "f", "g", "h", "i",
        ];
        let parsed = supervise(
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
            None,
        );
        assert!(
            !wrapped
                .args
                .iter()
                .any(|arg| arg.to_string_lossy().contains("next")),
            "{wrapped:?}"
        );
        let args = wrapped.args.clone();
        let parsed = supervise(args.into_iter().skip(1), wrapped_env(wrapped))?;
        assert!(parsed.cleanup.is_none());
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
        assert!(supervise(os(&[]), env(&["sh"])).is_err());
        assert!(supervise(os(&["--supervisor-pid", "1"]), env(&[])).is_err());
        assert!(supervise(os(&["--supervisor-pid", "1", "--bogus"]), env(&["sh"])).is_err());
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
        assert!(supervise(os(&["--supervisor-pid", "1"]), gapped.into_iter()).is_err());
        // A cleanup needs its timeout, and a cleanup-only shim needs no supervisor.
        assert!(cleanup_only(os(&["--cleanup-only"]), env(&["sh"])).is_err());
        assert!(
            cleanup_only(
                os(&["--cleanup-only", "--cleanup-timeout-ms", "10"]),
                env(&["sh"])
            )
            .is_ok()
        );
    }

    /// A service shim receives its service's cleanup next to the command, and neither command
    /// inherits the variables that carried them.
    #[test]
    fn service_cleanups_travel_to_the_shim() -> Result<(), String> {
        let cleanup = cleanup_spec(true);
        let wrapped = under(
            Path::new("/bin/micromux"),
            Path::new("docker"),
            &["compose".to_string(), "up".to_string()],
            StopPolicy::default(),
            Some(&cleanup),
        );
        assert!(
            !wrapped
                .args
                .iter()
                .any(|arg| arg.to_string_lossy().contains("down")),
            "{wrapped:?}"
        );
        let args = wrapped.args.clone();
        let parsed = supervise(args.into_iter().skip(1), wrapped_env(wrapped))?;
        let parsed_cleanup = parsed
            .cleanup
            .ok_or_else(|| "the cleanup was lost".to_string())?;
        assert_eq!(
            parsed_cleanup.command,
            ["docker", "compose", "down"].map(OsString::from).to_vec()
        );
        assert_eq!(parsed_cleanup.timeout, cleanup.timeout);
        assert_eq!(
            parsed_cleanup.log.as_deref(),
            Some(Path::new("/state/cleanup/key/infra.log"))
        );
        assert_eq!(
            parsed_cleanup.lock.as_deref(),
            Some(Path::new("/state/cleanup/key/infra.lock"))
        );
        // Both commands' carriers, three each, are kept out of either command's environment.
        assert_eq!(parsed.carriers.len(), 6);
        Ok(())
    }

    /// A cleanup-only shim gets the cleanup as its command and the service's stop signal, and runs
    /// without a log or a lock when the user has no state directory.
    #[test]
    fn cleanup_only_shims_carry_the_cleanup() -> Result<(), String> {
        let wrapped = cleanup_under(
            Path::new("/bin/micromux"),
            &cleanup_spec(false),
            crate::spec::StopSignal::Int,
        );
        let args = wrapped.args.clone();
        let parsed = cleanup_only(args.into_iter().skip(1), wrapped_env(wrapped))?;
        assert_eq!(parsed.stop_signal, nix::sys::signal::Signal::SIGINT);
        assert_eq!(
            parsed.cleanup.command,
            ["docker", "compose", "down"].map(OsString::from).to_vec()
        );
        assert_eq!(parsed.cleanup.timeout, Duration::from_secs(120));
        assert_eq!(parsed.cleanup.log, None);
        assert_eq!(parsed.cleanup.lock, None);
        assert_eq!(parsed.carriers.len(), 3);
        Ok(())
    }
}
