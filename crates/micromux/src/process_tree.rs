//! Best-effort enumeration of the live descendants of a process on Unix.
//!
//! Terminating a service signals its process group, but descendants that move into
//! their own groups via `setsid`/`setpgid` — as task runners like turborepo and
//! daemonizing dev servers do — are unreachable through `killpg`. Walking the
//! parent-pid table at kill time lets termination also reach those escapees.
//!
//! The walk is a snapshot: processes that fork between enumeration and signal
//! delivery can be missed, so callers must treat the result as an additive sweep on
//! top of the process-group kill, not as a containment guarantee.

use std::collections::{HashMap, HashSet, VecDeque};

/// Returns the pids of all live descendants of `root`, excluding `root` itself.
///
/// The result crosses process-group and session boundaries because it follows parent
/// pids, in breadth-first order from `root`.
///
/// Enumeration failures degrade to an empty result rather than an error: the caller's
/// process-group kill still applies, and termination must never abort because the
/// process table could not be read.
pub(crate) fn descendant_pids(root: u32) -> Vec<u32> {
    let mut children_by_parent: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, parent) in live_pid_parents() {
        children_by_parent.entry(parent).or_default().push(pid);
    }

    let mut descendants = Vec::new();
    let mut visited = HashSet::from([root]);
    let mut frontier = VecDeque::from([root]);
    while let Some(pid) = frontier.pop_front() {
        for &child in children_by_parent.get(&pid).into_iter().flatten() {
            // Pid reuse can make a stale snapshot self-referential; visit each pid once.
            if visited.insert(child) {
                descendants.push(child);
                frontier.push_back(child);
            }
        }
    }
    descendants
}

/// Returns `(pid, parent pid)` for every process visible through libproc.
#[cfg(target_os = "macos")]
fn live_pid_parents() -> Vec<(u32, u32)> {
    use nix::libc;
    use std::mem::MaybeUninit;

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
    let mut pids = vec![0 as libc::pid_t; count.saturating_add(64)];
    let Ok(buffer_bytes) = i32::try_from(std::mem::size_of_val(pids.as_slice())) else {
        return Vec::new();
    };
    #[expect(
        unsafe_code,
        reason = "`pids` is valid for writes of `buffer_bytes` bytes and \
                  proc_listallpids writes at most that many"
    )]
    let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), buffer_bytes) };
    pids.truncate(usize::try_from(filled).unwrap_or(0).min(pids.len()));

    let Ok(info_size) = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()) else {
        return Vec::new();
    };
    let mut pairs = Vec::with_capacity(pids.len());
    for pid in pids {
        if pid <= 0 {
            continue;
        }
        let mut info = MaybeUninit::<libc::proc_bsdinfo>::uninit();
        #[expect(
            unsafe_code,
            reason = "`info` is valid for writes of `info_size` bytes as \
                      PROC_PIDTBSDINFO requires"
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
        // Processes can exit or deny inspection between the two calls; skip those.
        if written != info_size {
            continue;
        }
        #[expect(
            unsafe_code,
            reason = "a full-size PROC_PIDTBSDINFO write initializes every field of \
                      proc_bsdinfo"
        )]
        let info = unsafe { info.assume_init() };
        if let Ok(pid) = u32::try_from(pid) {
            pairs.push((pid, info.pbi_ppid));
        }
    }
    pairs
}

/// Returns `(pid, parent pid)` for every process listed in `/proc`.
#[cfg(target_os = "linux")]
fn live_pid_parents() -> Vec<(u32, u32)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut pairs = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|name| name.parse::<u32>().ok()) else {
            continue;
        };
        // Processes can exit between the directory listing and the stat read.
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if let Some(parent) = parse_stat_parent_pid(&stat) {
            pairs.push((pid, parent));
        }
    }
    pairs
}

/// Fallback for Unix platforms without a supported process-table source.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn live_pid_parents() -> Vec<(u32, u32)> {
    Vec::new()
}

/// Extracts the parent pid (field 4) from `/proc/<pid>/stat` contents.
///
/// The second field (`comm`) may itself contain spaces and parentheses, so fields are
/// counted from the last `)` rather than from the start of the line.
#[cfg(any(target_os = "linux", test))]
fn parse_stat_parent_pid(stat: &str) -> Option<u32> {
    let (_, after_comm) = stat.rsplit_once(')')?;
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use color_eyre::eyre;
    use similar_asserts::assert_eq;

    use super::{descendant_pids, parse_stat_parent_pid};

    /// A `comm` containing spaces and a closing parenthesis must not shift the parent
    /// pid field.
    #[test]
    fn parses_parent_pid_from_stat_with_hostile_comm() {
        assert_eq!(
            parse_stat_parent_pid("1234 (tmux: server) S 1 1234 1234 0 -1"),
            Some(1)
        );
        assert_eq!(parse_stat_parent_pid("garbage"), None);
    }

    /// Background jobs under `set -m` run in their own process groups, mimicking task
    /// runners that escape the service's group; the parent-pid walk must still find
    /// them.
    #[test]
    fn finds_descendants_that_left_the_process_group() -> eyre::Result<()> {
        use std::os::unix::process::CommandExt as _;

        let mut child = std::process::Command::new("bash")
            .args(["-c", "set -m; sleep 30 & sleep 30 & wait"])
            .process_group(0)
            .spawn()?;
        let root = child.id();

        // The shell forks the background jobs asynchronously; wait for both to appear.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut found = descendant_pids(root);
        while found.len() < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
            found = descendant_pids(root);
        }

        // Kill the escaped sleeps and the shell's group before asserting so a failing
        // run does not leak processes.
        for &pid in &found {
            if let Ok(pid) = i32::try_from(pid) {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
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
        Ok(())
    }
}
