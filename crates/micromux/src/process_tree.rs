//! Best-effort enumeration and pid-reuse-safe signaling of process descendants on Unix.
//!
//! Terminating a service signals its process group, but descendants that move into
//! their own groups via `setsid`/`setpgid` — as task runners like turborepo do — are
//! unreachable through `killpg`. Walking the parent-pid table lets termination also
//! reach those escapees.
//!
//! Every observation is a [`ProcessStamp`]: the pid plus the platform's process start
//! time. Signal delivery re-verifies the stamp, so a pid recycled between enumeration
//! and delivery is skipped instead of signaled. On Linux delivery goes through a pidfd,
//! which closes the remaining verify-to-deliver window; on macOS a small window is
//! unavoidable because no pidfd equivalent exists.
//!
//! # Limitations
//!
//! Enumeration only works while the root is alive. The kernel reparents a process's
//! children when it exits, so a walk rooted at a dead process finds nothing — callers
//! must snapshot descendants *before* the root dies and retain the result, rather than
//! re-walking afterwards. [`expand_survivors`] partially recovers after the fact: a
//! retained process-group id stays valid while any member lives, so a membership scan
//! can still reach workers whose forker is already gone.
//!
//! A process that was already orphaned before the walk is unreachable, because no
//! parent chain leads back to the root. Daemons that double-fork at startup are the
//! common case: they reparent to init long before termination, so this module never
//! sees them. What it does reach is descendants whose parent chain to the root is
//! intact, whatever process groups or sessions they moved into.
//!
//! The walk is also a snapshot, not a containment guarantee: processes forked during
//! enumeration can be missed, and ancestry is joined by numeric parent pid, so a pid
//! recycled mid-walk can in principle graft an unrelated subtree. Treat the result as
//! an additive sweep on top of the process-group kill.
//!
//! On Linux the walk trusts the mounted `/proc` to describe the caller's own PID
//! namespace. When it does not — a container that inherited the host's procfs — the
//! numbers name different processes locally, so the sweep disables itself rather than
//! signal them.

use std::collections::{HashMap, HashSet, VecDeque};

use nix::sys::signal::Signal;

/// Identity of a process at observation time.
///
/// Two equal stamps refer to the same process incarnation, within the resolution of the
/// platform's start-time clock: microseconds on macOS, clock ticks (~10ms) on Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessStamp {
    /// Observed process id.
    pub(crate) pid: u32,
    /// Platform start-time cookie: clock ticks since boot on Linux, the start timeval
    /// on macOS. `None` on platforms without a supported process-table source.
    start_time: Option<(u64, u64)>,
}

/// A live descendant observed during a walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Descendant {
    /// Identity used to verify the process before signaling it.
    pub(crate) stamp: ProcessStamp,
    /// Process group at observation time, used to skip members a `killpg` already
    /// covered. `None` when the platform could not report it.
    pub(crate) process_group: Option<u32>,
}

/// Observes the current identity of `pid`, or `None` when it is gone or unreadable.
///
/// A zombie counts as gone: it has already terminated and only awaits reaping.
pub(crate) fn stamp(pid: u32) -> Option<ProcessStamp> {
    stamp_impl(pid)
}

/// Whether the process observed as `target` is still that same live process incarnation.
pub(crate) fn is_alive(target: ProcessStamp) -> bool {
    stamp(target.pid) == Some(target)
}

/// Returns every live descendant of `root`, excluding `root` itself.
///
/// The result crosses process-group and session boundaries because it follows parent
/// pids, in breadth-first order from `root`.
///
/// Returns empty when `root` is not alive — including when it died partway through the
/// walk — rather than risking a walk of an unrelated process's tree. Because the kernel
/// reparents children on exit, this also means a walk is only useful while `root` lives;
/// see the module docs.
///
/// Enumeration failures degrade to an empty result rather than an error: the caller's
/// process-group kill still applies, and termination must never abort because the
/// process table could not be read.
pub(crate) fn descendants(root: ProcessStamp) -> Vec<Descendant> {
    if !is_alive(root) {
        return Vec::new();
    }

    let mut children_by_parent: HashMap<u32, Vec<Descendant>> = HashMap::new();
    for record in live_processes() {
        children_by_parent
            .entry(record.parent)
            .or_default()
            .push(Descendant {
                stamp: ProcessStamp {
                    pid: record.pid,
                    start_time: record.start_time,
                },
                process_group: record.process_group,
            });
    }

    // The table is not read atomically. If the root died during the read, its children
    // have already reparented and any rows still claiming it as a parent belong to a
    // recycled pid, so the whole walk is untrustworthy.
    if !is_alive(root) {
        return Vec::new();
    }

    let mut result = Vec::new();
    let mut visited = HashSet::from([root.pid]);
    let mut frontier = VecDeque::from([root.pid]);
    while let Some(pid) = frontier.pop_front() {
        for &child in children_by_parent.get(&pid).into_iter().flatten() {
            // Pid reuse can make a stale snapshot self-referential; visit each pid once.
            if visited.insert(child.stamp.pid) {
                result.push(child);
                frontier.push_back(child.stamp.pid);
            }
        }
    }
    result
}

/// Expands a retained survivor set to descendants forked after the snapshot was taken.
///
/// One process-table read yields two kinds of roots: every still-live seed, and live
/// members of the recorded process `groups` (workers whose forker already died and
/// whose parent chain is therefore broken, but whose group id stays valid while any
/// member lives). A single breadth-first traversal from that union then adds their
/// live descendants — a group-recovered worker's own children count even when they
/// moved to yet another group. Dead seeds are dropped; the result includes the roots
/// and is deduplicated by pid.
///
/// Group membership shares `killpg`'s staleness risk: if an entire recorded group died
/// and its id was recycled for an unrelated group, the members matched here are
/// strangers. That requires a full pid-space wrap within a stop's grace window — the
/// same accepted residual as signaling any snapshotted group id.
pub(crate) fn expand_survivors(seeds: &[ProcessStamp], groups: &HashSet<u32>) -> Vec<ProcessStamp> {
    if seeds.is_empty() && groups.is_empty() {
        return Vec::new();
    }
    let own_pid = std::process::id();
    let mut children_by_parent: HashMap<u32, Vec<ProcessStamp>> = HashMap::new();
    let mut group_members: Vec<ProcessStamp> = Vec::new();
    for record in live_processes() {
        // Never signal the supervisor itself, however stale the group data is.
        if record.pid == own_pid {
            continue;
        }
        let stamp = ProcessStamp {
            pid: record.pid,
            start_time: record.start_time,
        };
        children_by_parent
            .entry(record.parent)
            .or_default()
            .push(stamp);
        if record
            .process_group
            .is_some_and(|group| groups.contains(&group))
        {
            group_members.push(stamp);
        }
    }

    let mut result = Vec::new();
    let mut visited = HashSet::new();
    let mut frontier = VecDeque::new();
    for &seed in seeds {
        // Re-verified after the table read, mirroring `descendants`: rows claiming a
        // dead seed as their parent belong to a recycled pid.
        if !is_alive(seed) {
            continue;
        }
        if visited.insert(seed.pid) {
            result.push(seed);
            frontier.push_back(seed.pid);
        }
    }
    // Group members join the frontier, not just the result: their children may sit in
    // yet another group and are only reachable by walking down from them.
    for member in group_members {
        if visited.insert(member.pid) {
            result.push(member);
            frontier.push_back(member.pid);
        }
    }
    while let Some(pid) = frontier.pop_front() {
        for &child in children_by_parent.get(&pid).into_iter().flatten() {
            if visited.insert(child.pid) {
                result.push(child);
                frontier.push_back(child.pid);
            }
        }
    }
    result
}

/// Delivers `signal` to `target` only if it is still the stamped process incarnation.
///
/// Returns whether the signal was delivered. A recycled, exited, or unreachable pid is
/// skipped and reported as not delivered.
#[cfg(target_os = "linux")]
pub(crate) fn send_signal(target: ProcessStamp, signal: Signal) -> bool {
    let Some(pid) = i32::try_from(target.pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    // Open first, verify second: the fd is pinned to whatever process owns the pid at
    // open time, so a positive identity check afterwards proves the fd refers to the
    // stamped process and the signal cannot hit a recycled pid.
    match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
        Ok(fd) => {
            if !is_alive(target) {
                return false;
            }
            let Some(mapped) = rustix_signal(signal) else {
                return verified_kill(target, signal);
            };
            match rustix::process::pidfd_send_signal(&fd, mapped) {
                Ok(()) => true,
                Err(rustix::io::Errno::SRCH) => false,
                Err(err) => {
                    tracing::debug!(
                        ?err,
                        pid = target.pid,
                        ?signal,
                        "pidfd signal failed; retrying with kill"
                    );
                    verified_kill(target, signal)
                }
            }
        }
        Err(rustix::io::Errno::SRCH) => false,
        // Every other error still leaves plain kill available: pre-5.3 kernels return
        // ENOSYS, container seccomp profiles that do not allowlist pidfd return EPERM,
        // and fd exhaustion returns EMFILE/ENFILE precisely when a runaway tree is being
        // killed. Without this fallback the sweep would silently do nothing.
        Err(err) => {
            tracing::debug!(
                ?err,
                pid = target.pid,
                "pidfd_open failed; falling back to kill"
            );
            verified_kill(target, signal)
        }
    }
}

/// Delivers `signal` to `target` only if it is still the stamped process incarnation.
///
/// Returns whether the signal was delivered. Without a pidfd equivalent, a small
/// verify-to-deliver window remains on this platform.
#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn send_signal(target: ProcessStamp, signal: Signal) -> bool {
    verified_kill(target, signal)
}

/// Re-verifies the stamp and delivers the signal with plain `kill`.
fn verified_kill(target: ProcessStamp, signal: Signal) -> bool {
    if !is_alive(target) {
        return false;
    }
    let Ok(pid) = i32::try_from(target.pid) else {
        return false;
    };
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), signal) {
        Ok(()) => true,
        Err(nix::errno::Errno::ESRCH) => false,
        Err(err) => {
            tracing::debug!(
                ?err,
                pid = target.pid,
                ?signal,
                "failed to signal descendant"
            );
            false
        }
    }
}

/// Maps the closed set of termination signals onto rustix for pidfd delivery.
#[cfg(target_os = "linux")]
fn rustix_signal(signal: Signal) -> Option<rustix::process::Signal> {
    match signal {
        Signal::SIGTERM => Some(rustix::process::Signal::TERM),
        Signal::SIGINT => Some(rustix::process::Signal::INT),
        Signal::SIGHUP => Some(rustix::process::Signal::HUP),
        Signal::SIGQUIT => Some(rustix::process::Signal::QUIT),
        Signal::SIGUSR1 => Some(rustix::process::Signal::USR1),
        Signal::SIGUSR2 => Some(rustix::process::Signal::USR2),
        Signal::SIGKILL => Some(rustix::process::Signal::KILL),
        _ => None,
    }
}

/// One row of the live process table.
struct ProcessRecord {
    pid: u32,
    parent: u32,
    process_group: Option<u32>,
    start_time: Option<(u64, u64)>,
}

#[cfg(target_os = "macos")]
fn stamp_impl(pid: u32) -> Option<ProcessStamp> {
    let pid = i32::try_from(pid).ok()?;
    let info = bsdinfo(pid)?;
    Some(ProcessStamp {
        pid: u32::try_from(pid).ok()?,
        start_time: Some((info.pbi_start_tvsec, info.pbi_start_tvusec)),
    })
}

/// Reads the BSD process-info record for `pid`.
///
/// Returns `None` when the process is gone, is a zombie, or denies inspection —
/// `proc_pidinfo` is uid-gated, so processes owned by another user are invisible.
#[cfg(target_os = "macos")]
fn bsdinfo(pid: i32) -> Option<nix::libc::proc_bsdinfo> {
    use nix::libc;
    use std::mem::MaybeUninit;

    let Ok(info_size) = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()) else {
        return None;
    };
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
    #[expect(
        unsafe_code,
        reason = "`info` is valid for writes of `info_size` bytes as PROC_PIDTBSDINFO \
                  requires"
    )]
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            info_size,
        )
    };
    // A short write means the process exited or denied inspection.
    if written != info_size {
        return None;
    }
    #[expect(
        unsafe_code,
        reason = "a full-size PROC_PIDTBSDINFO write initializes every field of \
                  proc_bsdinfo"
    )]
    let info = unsafe { info.assume_init() };
    // A zombie has already terminated; treating it as live would make termination
    // re-signal a corpse and then report it as having survived.
    if info.pbi_status == libc::SZOMB {
        return None;
    }
    Some(info)
}

/// Lists every pid visible through libproc, growing the buffer until it is not truncated.
///
/// `proc_listallpids` fills at most the supplied capacity and reports no overflow, so a
/// result that exactly fills the buffer may have silently dropped entries.
#[cfg(target_os = "macos")]
fn list_all_pids() -> Vec<nix::libc::pid_t> {
    use nix::libc;

    #[expect(
        unsafe_code,
        reason = "libproc is the only unprivileged process-table source on macOS; a \
                  null buffer makes proc_listallpids return the current pid count \
                  without writing anywhere"
    )]
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };

    // Headroom absorbs processes forked between the sizing call and the fill call.
    let mut capacity = count.saturating_add(64);
    for _ in 0..4 {
        let mut pids = vec![0 as libc::pid_t; capacity];
        let Ok(buffer_bytes) = i32::try_from(std::mem::size_of_val(pids.as_slice())) else {
            return Vec::new();
        };
        #[expect(
            unsafe_code,
            reason = "`pids` is valid for writes of `buffer_bytes` bytes and \
                      proc_listallpids writes at most that many"
        )]
        let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), buffer_bytes) };
        let Ok(filled) = usize::try_from(filled) else {
            return Vec::new();
        };
        // A strictly shorter fill proves the buffer was large enough.
        if filled < pids.len() {
            pids.truncate(filled);
            return pids;
        }
        capacity = capacity.saturating_mul(2);
    }
    tracing::debug!("process table kept growing while enumerating; sweep may be incomplete");
    Vec::new()
}

/// Returns the live process table for every process visible through libproc.
#[cfg(target_os = "macos")]
fn live_processes() -> Vec<ProcessRecord> {
    let pids = list_all_pids();
    let mut records = Vec::with_capacity(pids.len());
    let mut unreadable = 0usize;
    for pid in pids {
        if pid <= 0 {
            continue;
        }
        // Processes can exit, become zombies, or deny inspection between the two calls.
        // A denied process also breaks the parent chain through it, hiding its own
        // children from the walk; nothing unprivileged can recover that on macOS.
        let Some(info) = bsdinfo(pid) else {
            unreadable = unreadable.saturating_add(1);
            continue;
        };
        if let Ok(pid) = u32::try_from(pid) {
            records.push(ProcessRecord {
                pid,
                parent: info.pbi_ppid,
                process_group: Some(info.pbi_pgid),
                start_time: Some((info.pbi_start_tvsec, info.pbi_start_tvusec)),
            });
        }
    }
    if unreadable > 0 {
        tracing::debug!(
            unreadable,
            visible = records.len(),
            "some processes could not be inspected; their children are hidden from the sweep"
        );
    }
    records
}

/// Whether the mounted `/proc` describes the caller's own PID namespace.
///
/// A container that inherits the host's procfs reports host pids; interpreting those
/// numbers in the container's namespace would verify one process and then signal an
/// unrelated one, so the sweep is disabled entirely when the two views disagree.
#[cfg(target_os = "linux")]
fn proc_matches_pid_namespace() -> bool {
    static MATCHES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *MATCHES.get_or_init(|| {
        let observed = std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|stat| stat.split_whitespace().next()?.parse::<u32>().ok());
        let matches = observed == Some(std::process::id());
        if !matches {
            tracing::warn!(
                ?observed,
                own = std::process::id(),
                "/proc reports another pid namespace; descendant sweep disabled"
            );
        }
        matches
    })
}

#[cfg(target_os = "linux")]
fn stamp_impl(pid: u32) -> Option<ProcessStamp> {
    if !proc_matches_pid_namespace() {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = parse_stat_fields(&stat)?;
    // A zombie has already terminated; treating it as live would make termination
    // re-signal a corpse and then report it as having survived.
    if fields.zombie {
        return None;
    }
    Some(ProcessStamp {
        pid,
        start_time: Some((fields.start_time, 0)),
    })
}

/// Returns the live process table for every process listed in `/proc`.
#[cfg(target_os = "linux")]
fn live_processes() -> Vec<ProcessRecord> {
    if !proc_matches_pid_namespace() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir("/proc") else {
        tracing::debug!("could not read /proc; descendant sweep is unavailable");
        return Vec::new();
    };
    let mut records = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        // Processes can exit between the directory listing and the stat read.
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(fields) = parse_stat_fields(&stat) else {
            continue;
        };
        if fields.zombie {
            continue;
        }
        records.push(ProcessRecord {
            pid,
            parent: fields.parent,
            process_group: Some(fields.process_group),
            start_time: Some((fields.start_time, 0)),
        });
    }
    records
}

/// Fallback for Unix platforms without a supported process-table source.
///
/// [`stamp`] returning `None` here disables the descendant sweep entirely, leaving the
/// process-group kill as the only mechanism.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn stamp_impl(_pid: u32) -> Option<ProcessStamp> {
    None
}

/// Fallback for Unix platforms without a supported process-table source.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn live_processes() -> Vec<ProcessRecord> {
    Vec::new()
}

/// The `/proc/<pid>/stat` fields this module needs.
#[cfg(any(target_os = "linux", test))]
struct StatFields {
    parent: u32,
    process_group: u32,
    start_time: u64,
    zombie: bool,
}

/// Extracts the state, parent pid, process group, and start time from
/// `/proc/<pid>/stat` contents — fields 3, 4, 5, and 22.
///
/// The second field (`comm`) may itself contain spaces and parentheses, so fields are
/// counted from the last `)` rather than from the start of the line.
#[cfg(any(target_os = "linux", test))]
fn parse_stat_fields(stat: &str) -> Option<StatFields> {
    let (_, after_comm) = stat.rsplit_once(')')?;
    let mut fields = after_comm.split_whitespace();
    let state = fields.next()?;
    let parent = fields.next()?.parse().ok()?;
    let process_group = fields.next()?.parse().ok()?;
    // The start time is field 22 overall. State, ppid, and pgrp are already consumed,
    // so 16 fields separate the cursor from it.
    let start_time = fields.nth(16)?.parse().ok()?;
    Some(StatFields {
        parent,
        process_group,
        start_time,
        zombie: state == "Z",
    })
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre::{self, OptionExt as _};
    use similar_asserts::assert_eq;

    use super::{descendants, expand_survivors, is_alive, parse_stat_fields, stamp};

    /// A `comm` containing spaces and a closing parenthesis must not shift the parent
    /// pid, process group, or start-time fields.
    #[test]
    fn parses_stat_fields_with_hostile_comm() -> eyre::Result<()> {
        // Fields after `)`: state, ppid, pgrp, session, tty, tpgid, flags, minflt,
        // cminflt, majflt, cmajflt, utime, stime, cutime, cstime, priority, nice,
        // num_threads, itrealvalue, starttime.
        let stat = "1234 (tmux: server) S 1 4321 1234 0 -1 4194304 5 0 0 0 2 1 0 0 20 0 1 0 4242 0";
        let fields = parse_stat_fields(stat).ok_or_eyre("expected the stat line to parse")?;
        assert_eq!(fields.parent, 1);
        assert_eq!(fields.process_group, 4321);
        assert_eq!(fields.start_time, 4242);
        assert!(!fields.zombie);

        // A zombie must be reported so callers can treat it as already dead.
        let zombie = "9 (defunct proc) Z 1 9 9 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 77 0";
        let fields = parse_stat_fields(zombie).ok_or_eyre("expected the zombie line to parse")?;
        assert!(fields.zombie);

        assert!(parse_stat_fields("garbage").is_none());
        Ok(())
    }

    /// A stamp taken twice for a live process must observe the same identity, a stamp
    /// whose start time differs must be rejected even though the pid is live, and a
    /// stamp must stop matching once the process is gone.
    ///
    /// The middle case is what makes pid reuse safe: without it, `is_alive` would be a
    /// bare existence check and the start-time half of the stamp would carry no weight.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn stamps_identify_process_incarnations() -> eyre::Result<()> {
        let mut child = std::process::Command::new("sleep").arg("30").spawn()?;
        let root = stamp(child.id()).ok_or_eyre("expected a stamp")?;
        assert!(is_alive(root));

        // Forge the observation a recycled pid would produce: same pid, different
        // start time. Signaling it must be refused rather than hitting the live
        // process that currently owns the pid.
        let mut recycled = root;
        recycled.start_time = Some((
            root.start_time
                .map_or(1, |(seconds, _)| seconds.wrapping_add(1)),
            0,
        ));
        assert!(!is_alive(recycled));
        assert!(!super::send_signal(
            recycled,
            nix::sys::signal::Signal::SIGKILL
        ));
        assert!(
            child.try_wait()?.is_none(),
            "a mismatched stamp must not deliver a signal"
        );

        child.kill()?;
        child.wait()?;
        // The identity check must fail after exit even if the pid were recycled.
        assert!(!is_alive(root));
        Ok(())
    }

    /// The synthetic fixture above can drift from the kernel's actual layout, so anchor
    /// the field offsets against a real `/proc` entry. A wrong offset landing on
    /// `itrealvalue` (always 0) would silently degrade every stamp to a pid-existence
    /// check without failing any other test.
    #[cfg(target_os = "linux")]
    #[test]
    fn parses_stat_fields_of_the_running_process() -> eyre::Result<()> {
        let stat = std::fs::read_to_string("/proc/self/stat")?;
        let fields = parse_stat_fields(&stat).ok_or_eyre("expected /proc/self/stat to parse")?;
        assert_eq!(
            fields.parent,
            u32::try_from(nix::unistd::getppid().as_raw())?
        );
        assert_eq!(
            fields.process_group,
            u32::try_from(nix::unistd::getpgrp().as_raw())?
        );
        assert!(
            fields.start_time > 0,
            "field 22 must be the nonzero starttime"
        );
        assert!(!fields.zombie);
        Ok(())
    }

    /// Background jobs under `set -m` run in their own process groups, mimicking task
    /// runners that escape the service's group; the parent-pid walk must still find
    /// them and report the group that lets callers skip a `killpg`-covered process.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn finds_descendants_that_left_the_process_group() -> eyre::Result<()> {
        use std::os::unix::process::CommandExt as _;

        let mut child = std::process::Command::new("bash")
            .args(["-c", "set -m; sleep 30 & sleep 30 & wait"])
            .process_group(0)
            .spawn()?;
        let root = child.id();
        let root_stamp = stamp(root).ok_or_eyre("expected a root stamp")?;

        // The shell forks the background jobs asynchronously; wait for both to appear.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut found = descendants(root_stamp);
        while found.len() < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
            found = descendants(root_stamp);
        }

        let escaped_the_group = found
            .iter()
            .filter(|descendant| descendant.process_group != Some(root))
            .count();

        // Kill the escaped sleeps and the shell's group before asserting so a failing
        // run does not leak processes.
        for target in &found {
            let _ = super::send_signal(target.stamp, nix::sys::signal::Signal::SIGKILL);
        }
        if let Ok(root) = i32::try_from(root) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(root),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = child.kill();
        let _ = child.wait();

        eyre::ensure!(
            found.len() >= 2,
            "expected the two escaped sleeps under the shell, found {found:?}"
        );
        eyre::ensure!(
            escaped_the_group >= 2,
            "expected both sleeps to report a group other than the shell's, found {found:?}"
        );
        Ok(())
    }

    /// The expansion must recover processes a retained snapshot does not name: live
    /// descendants of a surviving seed, and members of a recorded process group even
    /// when no seed is passed at all — including *their* children in other groups,
    /// which requires the traversal to continue from group-recovered roots. A dead
    /// seed must be dropped, not walked.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn expansion_recovers_descendants_and_group_members() -> eyre::Result<()> {
        use std::collections::HashSet;
        use std::os::unix::process::CommandExt as _;

        // `process_group(0)` makes the shell's pgid its own pid, so group membership
        // can be probed by that number. Under `set -m` the sleep moves to its *own*
        // group: from a group-only probe it is reachable solely by walking down from
        // the recovered shell, which is exactly the traversal under test.
        let mut child = std::process::Command::new("bash")
            .args(["-c", "set -m; sleep 30 & wait"])
            .process_group(0)
            .spawn()?;
        let root = child.id();
        let root_stamp = stamp(root).ok_or_eyre("expected a root stamp")?;

        // The shell forks the sleep asynchronously; wait for it to join the tree.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut from_seed = expand_survivors(&[root_stamp], &HashSet::new());
        while from_seed.len() < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
            from_seed = expand_survivors(&[root_stamp], &HashSet::new());
        }
        let from_group = expand_survivors(&[], &HashSet::from([root]));

        // Kill the fixture before asserting so a failing run does not leak processes;
        // the escaped sleep is outside the root's group, so it is killed by stamp.
        for &survivor in &from_seed {
            let _ = super::send_signal(survivor, nix::sys::signal::Signal::SIGKILL);
        }
        if let Ok(root) = i32::try_from(root) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(root),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        let _ = child.kill();
        let _ = child.wait();
        // Wait for the kills to land so no fixture process outlives the test.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while from_seed.iter().any(|&survivor| is_alive(survivor))
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let after_death = expand_survivors(&[root_stamp], &HashSet::new());

        eyre::ensure!(
            from_seed.len() >= 2 && from_seed.contains(&root_stamp),
            "expected the live seed plus its forked sleep, found {from_seed:?}"
        );
        eyre::ensure!(
            from_group.len() >= 2,
            "expected the group-only probe to recover the shell and, through it, the \
             sleep that escaped to another group, found {from_group:?}"
        );
        eyre::ensure!(
            after_death.is_empty(),
            "a dead seed with no groups must expand to nothing, found {after_death:?}"
        );
        Ok(())
    }

    /// A walk rooted at a dead process must return nothing rather than trusting parent
    /// pids that may since have been recycled.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn walk_from_a_dead_root_is_empty() -> eyre::Result<()> {
        let mut child = std::process::Command::new("sleep").arg("30").spawn()?;
        let root = stamp(child.id()).ok_or_eyre("expected a stamp")?;
        child.kill()?;
        child.wait()?;

        assert!(descendants(root).is_empty());
        Ok(())
    }
}
