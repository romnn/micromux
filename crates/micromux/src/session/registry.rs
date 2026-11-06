//! The per-user registry of session-allocated ports.
//!
//! Each session that allocates ports owns two files in the registry directory, keyed by the
//! digest of its canonical config path:
//!
//! - `<key>.json` persists the assignment, so a restart gets the same ports back.
//! - `<key>.lock` is held with an exclusive lock for the session's whole lifetime.
//!
//! The kernel releases a lock when its holder dies, SIGKILL included, so a lock that can be taken
//! proves its owner is gone; a missing or stale record proves nothing.
//! Every read, liveness probe, and write happens under the registry-wide `registry.lock`, which
//! serializes allocation between sessions starting at the same time.
//! A lock file is only removed together with the record of a dead session whose config file no
//! longer exists, and only under the registry lock: replacing a lock file someone could still
//! open outside it would let two processes hold "the" lock at once.

use std::collections::HashSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::{SessionError, config_path_key};

/// Ports handed out to sessions that configure no `session.port_range`.
///
/// The range sits below the ephemeral ranges of Linux (32768+) and macOS (49152+), so outgoing
/// connections never take a port a session has been promised.
pub(crate) const DEFAULT_PORT_RANGE: RangeInclusive<u16> = 20000..=32767;

const REGISTRY_LOCK: &str = "registry.lock";

/// Persisted port assignment of one session.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    /// Canonical config path, for humans and conflict messages.
    config_path: String,
    /// Session name, which must be unique among live sessions.
    session_name: String,
    /// Allocated port per variable name.
    ports: IndexMap<String, u16>,
}

/// Keeps a session's ports reserved while held.
#[derive(Debug)]
pub(crate) struct Lease {
    #[expect(
        dead_code,
        reason = "held for RAII: the exclusive lock marks this session live in the registry"
    )]
    instance: File,
}

/// Where the per-user port registry lives.
#[derive(Debug, Clone, Default)]
pub struct PortRegistry {
    directory: Option<PathBuf>,
}

impl PortRegistry {
    /// The current user's registry, located when first used.
    #[must_use]
    pub fn user() -> Self {
        Self::default()
    }

    /// A registry in `directory`, for tests and isolated setups.
    #[must_use]
    pub fn at(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: Some(directory.into()),
        }
    }

    fn directory(&self) -> Result<PathBuf, SessionError> {
        let directory = match &self.directory {
            Some(directory) => directory.clone(),
            None => user_registry_directory().ok_or(SessionError::NoStateDirectory)?,
        };
        create_private_dir(&directory).map_err(|source| SessionError::Io {
            path: directory.clone(),
            source,
        })?;
        Ok(directory)
    }

    /// The assignment persisted for `config_path`, read without locking or reserving anything.
    ///
    /// Records are replaced atomically, so an unlocked read sees either the old or the new one.
    pub(crate) fn persisted(&self, config_path: &Path) -> Option<IndexMap<String, u16>> {
        let directory = match &self.directory {
            Some(directory) => directory.clone(),
            None => user_registry_directory()?,
        };
        let path = directory.join(format!("{}.json", config_path_key(config_path)));
        read_record(&path).ok().flatten().map(|record| record.ports)
    }

    /// Reserves a port in `range` for every name in `names`, reusing this session's persisted
    /// assignment where it still lies in `range`.
    ///
    /// The returned lease marks the session live until it is dropped.
    pub(crate) fn acquire(
        &self,
        config_path: &Path,
        session_name: &str,
        names: &[String],
        excluded: &[u16],
        range: &RangeInclusive<u16>,
    ) -> Result<(IndexMap<String, u16>, Lease), SessionError> {
        let directory = self.directory()?;
        let io = |path: &Path| {
            let path = path.to_path_buf();
            move |source| SessionError::Io { path, source }
        };

        // Serialize with every other session's allocation
        let registry_lock_path = directory.join(REGISTRY_LOCK);
        let registry_lock = open_lock_file(&registry_lock_path, true)
            .map_err(io(&registry_lock_path))?
            .ok_or_else(|| SessionError::Io {
                path: registry_lock_path.clone(),
                source: std::io::ErrorKind::NotFound.into(),
            })?;
        registry_lock.lock().map_err(io(&registry_lock_path))?;

        // Claim this session's own instance lock
        let key = config_path_key(config_path);
        let instance_path = directory.join(format!("{key}.lock"));
        let instance = open_lock_file(&instance_path, true)
            .map_err(io(&instance_path))?
            .ok_or_else(|| SessionError::Io {
                path: instance_path.clone(),
                source: std::io::ErrorKind::NotFound.into(),
            })?;
        match instance.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(SessionError::AlreadyRunning {
                    config_path: config_path.to_path_buf(),
                });
            }
            Err(TryLockError::Error(source)) => {
                return Err(SessionError::Io {
                    path: instance_path,
                    source,
                });
            }
        }

        // Survey the other sessions
        let survey = survey(&directory, &key, session_name)?;

        // Assign ports
        let record_path = directory.join(format!("{key}.json"));
        let persisted = match read_record(&record_path) {
            Ok(record) => record.map(|record| record.ports).unwrap_or_default(),
            Err(message) => {
                tracing::warn!(path = %record_path.display(), message, "ignoring unreadable port record");
                IndexMap::new()
            }
        };
        let mut ports = IndexMap::new();
        let mut taken: HashSet<u16> = survey.live.clone();
        taken.extend(excluded.iter().copied());
        for name in names {
            let port = match persisted.get(name).copied() {
                // A range changed since the assignment: a new one in the range replaces it.
                Some(port) if !range.contains(&port) => {
                    pick_port(&key, name, range, &taken, &survey.dormant)?
                }
                Some(port) if excluded.contains(&port) => {
                    return Err(SessionError::PortReserved {
                        variable: name.clone(),
                        port,
                        record: record_path,
                    });
                }
                Some(port)
                    if !survey.live.contains(&port) && !ports.values().any(|p| *p == port) =>
                {
                    if !port_is_free(port) {
                        return Err(SessionError::PortBusy {
                            variable: name.clone(),
                            port,
                        });
                    }
                    port
                }
                _ => pick_port(&key, name, range, &taken, &survey.dormant)?,
            };
            taken.insert(port);
            ports.insert(name.clone(), port);
        }

        // Commit before any service can start using the ports
        let record = Record {
            config_path: config_path.display().to_string(),
            session_name: session_name.to_string(),
            ports: ports.clone(),
        };
        write_record(&directory, &record_path, &record).map_err(io(&record_path))?;
        drop(registry_lock);
        Ok((ports, Lease { instance }))
    }
}

/// Ports claimed by other sessions.
struct Survey {
    /// Ports of sessions whose instance lock is held.
    live: HashSet<u16>,
    /// Ports persisted by sessions that are not running, reused only as a last resort so their
    /// next start finds its assignment intact.
    dormant: HashSet<u16>,
}

fn survey(directory: &Path, own_key: &str, session_name: &str) -> Result<Survey, SessionError> {
    let mut survey = Survey {
        live: HashSet::new(),
        dormant: HashSet::new(),
    };
    let entries = std::fs::read_dir(directory).map_err(|source| SessionError::Io {
        path: directory.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| SessionError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let Some(key) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        if key == own_key {
            continue;
        }
        let lock_path = directory.join(format!("{key}.lock"));
        let live = owner_is_live(&lock_path).map_err(|source| SessionError::Io {
            path: lock_path,
            source,
        })?;
        let record = match read_record(&path) {
            Ok(Some(record)) => record,
            Ok(None) => continue,
            Err(message) if live => {
                return Err(SessionError::UnreadableRecord { path, message });
            }
            Err(message) => {
                tracing::warn!(path = %path.display(), message, "ignoring unreadable port record");
                continue;
            }
        };
        // Only a definite "not found" prunes: a config that cannot be checked, say for lack of
        // permission, may well still exist.
        if !live && matches!(Path::new(&record.config_path).try_exists(), Ok(false)) {
            // The project is gone, so nobody can restart this session; drop its reservation.
            // Removing the lock file is safe here: instance locks are only ever taken under the
            // registry lock this function runs under, and this one is free.
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(directory.join(format!("{key}.lock")));
            continue;
        }
        if live {
            if record.session_name == session_name {
                return Err(SessionError::NameInUse {
                    name: session_name.to_string(),
                    other_config: record.config_path,
                });
            }
            survey.live.extend(record.ports.values().copied());
        } else {
            survey.dormant.extend(record.ports.values().copied());
        }
    }
    Ok(survey)
}

/// Picks a free port in `range` for `name`, preferring ports no other session has ever been
/// assigned.
///
/// The scan starts at an offset derived from the session and the variable, so a session tends to
/// get the same ports again even after its record is deleted.
fn pick_port(
    key: &str,
    name: &str,
    range: &RangeInclusive<u16>,
    taken: &HashSet<u16>,
    dormant: &HashSet<u16>,
) -> Result<u16, SessionError> {
    let start = *range.start();
    let len = u32::from(range.end().saturating_sub(start)) + 1;
    let seed = config_path_key(Path::new(&format!("{key}/{name}")));
    let offset = u32::from_str_radix(seed.get(..8).unwrap_or("0"), 16).unwrap_or(0) % len;
    let candidates = || {
        (0..len).filter_map(move |step| {
            u16::try_from((offset + step) % len)
                .ok()
                .map(|index| start + index)
        })
    };
    for avoid_dormant in [true, false] {
        if let Some(port) = candidates().find(|port| {
            let reserved = avoid_dormant && dormant.contains(port);
            !taken.contains(port) && !reserved && port_is_free(*port)
        }) {
            return Ok(port);
        }
    }
    Err(SessionError::Exhausted {
        variable: name.to_string(),
        start,
        end: *range.end(),
    })
}

/// Whether nothing currently listens on `port`, probed by binding the loopback and wildcard
/// addresses of both families.
///
/// Every address matters: with `SO_REUSEADDR`, macOS lets a specific-address bind succeed next to
/// a wildcard listener and vice versa, and Windows reports an exclusive wildcard listener as
/// "access denied".
/// Any failure therefore counts as busy, except IPv6 being unavailable on this host.
fn port_is_free(port: u16) -> bool {
    let ipv4 = [Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED]
        .into_iter()
        .all(|address| TcpListener::bind(SocketAddr::from((address, port))).is_ok());
    let ipv6 = [Ipv6Addr::LOCALHOST, Ipv6Addr::UNSPECIFIED]
        .into_iter()
        .all(
            |address| match TcpListener::bind(SocketAddr::from((address, port))) {
                Ok(_) => true,
                Err(err) => ipv6_unavailable(&err),
            },
        );
    ipv4 && ipv6
}

/// Whether binding failed because the host has no usable IPv6, rather than because the port is
/// taken.
fn ipv6_unavailable(err: &std::io::Error) -> bool {
    #[cfg(unix)]
    let unsupported = [
        nix::errno::Errno::EAFNOSUPPORT as i32,
        nix::errno::Errno::EPROTONOSUPPORT as i32,
    ];
    // WSAEAFNOSUPPORT and WSAEPROTONOSUPPORT
    #[cfg(not(unix))]
    let unsupported = [10047, 10043];
    err.kind() == std::io::ErrorKind::AddrNotAvailable
        || err
            .raw_os_error()
            .is_some_and(|code| unsupported.contains(&code))
}

/// Whether some process holds the instance lock at `path`.
///
/// Only called under the registry lock, which every session also holds while claiming its own
/// instance lock, so the brief probe lock can never make a starting session look like a
/// duplicate.
fn owner_is_live(path: &Path) -> std::io::Result<bool> {
    let Some(file) = open_lock_file(path, false)? else {
        return Ok(false);
    };
    match file.try_lock() {
        Ok(()) => {
            file.unlock()?;
            Ok(false)
        }
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

fn open_lock_file(path: &Path, create: bool) -> std::io::Result<Option<File>> {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(path)
    {
        Ok(file) => Ok(Some(file)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound && !create => Ok(None),
        Err(err) => Err(err),
    }
}

/// Reads a record, `Ok(None)` when there is none, and a message when it cannot be parsed.
fn read_record(path: &Path) -> Result<Option<Record>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|err| err.to_string()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

/// Replaces a record atomically and durably: a crash leaves either the old or the new record.
fn write_record(directory: &Path, path: &Path, record: &Record) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|err| err.error)?;
    #[cfg(unix)]
    File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn create_private_dir(directory: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
}

#[cfg(not(unix))]
fn create_private_dir(directory: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)
}

/// The current user's registry directory.
///
/// On Unix it derives from the account's home directory in the password database rather than
/// `$HOME` or `$XDG_STATE_HOME`, so sessions started from differently configured shells, agents,
/// or services still share one registry.
#[cfg(unix)]
fn user_registry_directory() -> Option<PathBuf> {
    let user = nix::unistd::User::from_uid(nix::unistd::Uid::current())
        .ok()
        .flatten()?;
    let state = if cfg!(target_os = "macos") {
        user.dir
            .join("Library/Application Support/com.romnn.micromux")
    } else {
        user.dir.join(".local/state/micromux")
    };
    Some(state.join("ports"))
}

#[cfg(not(unix))]
fn user_registry_directory() -> Option<PathBuf> {
    crate::project_dir().map(|directories| directories.data_local_dir().join("ports"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use similar_asserts::assert_eq;

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    /// A restarted session gets back exactly the ports it had, and a second live session never
    /// receives one of them.
    #[test]
    fn assignments_are_stable_and_disjoint() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path());
        let first = Path::new("/projects/one/micromux.yaml");
        let second = Path::new("/projects/two/micromux.yaml");

        let (ports, lease) =
            registry.acquire(first, "one", &names(&["A", "B"]), &[], &DEFAULT_PORT_RANGE)?;
        assert_eq!(ports.len(), 2);
        assert!(ports.values().all(|port| DEFAULT_PORT_RANGE.contains(port)));
        assert_ne!(ports.get("A"), ports.get("B"));

        // A concurrent session avoids every port the live one holds.
        let (other, other_lease) =
            registry.acquire(second, "two", &names(&["A", "B"]), &[], &DEFAULT_PORT_RANGE)?;
        assert!(
            other
                .values()
                .all(|port| !ports.values().any(|p| p == port))
        );

        // Restarting the first session reproduces its assignment.
        drop(lease);
        let (again, _lease) =
            registry.acquire(first, "one", &names(&["A", "B"]), &[], &DEFAULT_PORT_RANGE)?;
        assert_eq!(again, ports);
        drop(other_lease);
        Ok(())
    }

    /// A persisted port outside a newly configured range is replaced by one inside it, and the
    /// replacement persists; ports already inside are kept.
    #[test]
    fn assignments_outside_the_range_are_replaced() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path());
        let config = Path::new("/projects/one/micromux.yaml");

        let (before, lease) =
            registry.acquire(config, "one", &names(&["A"]), &[], &DEFAULT_PORT_RANGE)?;
        drop(lease);
        let range = 40000..=40999;
        let (after, lease) = registry.acquire(config, "one", &names(&["A"]), &[], &range)?;
        drop(lease);
        let (again, _lease) = registry.acquire(config, "one", &names(&["A"]), &[], &range)?;

        assert!(before.values().all(|port| !range.contains(port)));
        assert!(after.values().all(|port| range.contains(port)), "{after:?}");
        assert_eq!(again, after);
        Ok(())
    }

    /// A range too small for the names reports the range it ran out of.
    #[test]
    fn exhausted_ranges_are_reported() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path());
        let config = Path::new("/projects/one/micromux.yaml");
        // Hold the only port, so the range is exhausted whatever else runs on this machine.
        let holder = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        let port = holder.local_addr()?.port();

        let result = registry.acquire(config, "one", &names(&["A"]), &[], &(port..=port));

        assert!(
            matches!(result, Err(SessionError::Exhausted { start, end, .. }) if start == port && end == port),
            "{result:?}"
        );
        Ok(())
    }

    /// The instance lock makes a second supervisor for the same config fail instead of sharing
    /// its ports.
    #[test]
    fn second_instance_of_the_same_config_is_rejected() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path());
        let config = Path::new("/projects/one/micromux.yaml");

        let (_ports, _lease) =
            registry.acquire(config, "one", &names(&["A"]), &[], &DEFAULT_PORT_RANGE)?;
        let result = registry.acquire(config, "one", &names(&["A"]), &[], &DEFAULT_PORT_RANGE);

        assert!(matches!(result, Err(SessionError::AlreadyRunning { .. })));
        Ok(())
    }

    /// Two live sessions with one name would serve the same hostnames, so the second fails; once
    /// the first stops, the name is free again.
    #[test]
    fn live_sessions_need_distinct_names() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path());

        let (_ports, lease) = registry.acquire(
            Path::new("/a/app/micromux.yaml"),
            "app",
            &names(&["A"]),
            &[],
            &DEFAULT_PORT_RANGE,
        )?;
        let result = registry.acquire(
            Path::new("/b/app/micromux.yaml"),
            "app",
            &names(&["A"]),
            &[],
            &DEFAULT_PORT_RANGE,
        );
        assert!(matches!(result, Err(SessionError::NameInUse { .. })));

        drop(lease);
        registry.acquire(
            Path::new("/b/app/micromux.yaml"),
            "app",
            &names(&["A"]),
            &[],
            &DEFAULT_PORT_RANGE,
        )?;
        Ok(())
    }

    /// A persisted port that a foreign process now holds fails the start rather than pointing the
    /// session at that process.
    #[test]
    fn busy_persisted_port_fails_startup() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path());
        let config = Path::new("/projects/ipv4-busy/micromux.yaml");
        let (ports, lease) =
            registry.acquire(config, "one", &names(&["A"]), &[], &DEFAULT_PORT_RANGE)?;
        drop(lease);
        let port = *ports.get("A").ok_or_else(|| eyre::eyre!("no port"))?;

        let _foreign = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
        let result = registry.acquire(config, "one", &names(&["A"]), &[], &DEFAULT_PORT_RANGE);

        assert!(
            matches!(&result, Err(SessionError::PortBusy { variable, port: busy }) if variable == "A" && *busy == port),
            "{result:?}"
        );
        Ok(())
    }

    /// A listener on the IPv6 wildcard alone also makes a persisted port busy.
    #[test]
    fn busy_ipv6_wildcard_port_fails_startup() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path());
        let config = Path::new("/projects/ipv6-busy/micromux.yaml");
        let (ports, lease) =
            registry.acquire(config, "one", &names(&["A"]), &[], &DEFAULT_PORT_RANGE)?;
        drop(lease);
        let port = *ports.get("A").ok_or_else(|| eyre::eyre!("no port"))?;

        let Ok(_foreign) = TcpListener::bind((Ipv6Addr::UNSPECIFIED, port)) else {
            // No IPv6 on this host: nothing to probe.
            return Ok(());
        };
        let result = registry.acquire(config, "one", &names(&["A"]), &[], &DEFAULT_PORT_RANGE);

        assert!(
            matches!(&result, Err(SessionError::PortBusy { .. })),
            "{result:?}"
        );
        Ok(())
    }

    /// A stopped session whose project was deleted gives up its record, so temporary checkouts
    /// do not reserve ports forever.
    #[test]
    fn records_of_deleted_projects_are_pruned() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path().join("registry"));
        let project = directory.path().join("gone");
        std::fs::create_dir_all(&project)?;
        let config = project.join("micromux.yaml");
        std::fs::write(&config, "")?;
        let (_ports, lease) =
            registry.acquire(&config, "gone", &names(&["A"]), &[], &DEFAULT_PORT_RANGE)?;
        drop(lease);
        let record = directory
            .path()
            .join("registry")
            .join(format!("{}.json", config_path_key(&config)));
        assert!(record.exists());

        std::fs::remove_dir_all(&project)?;
        registry.acquire(
            Path::new("/projects/two/micromux.yaml"),
            "two",
            &[],
            &[],
            &DEFAULT_PORT_RANGE,
        )?;

        assert!(!record.exists());
        Ok(())
    }

    /// A config that cannot be checked, here for lack of permission, keeps its record.
    #[cfg(unix)]
    #[test]
    fn records_of_inaccessible_projects_are_kept() -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path().join("registry"));
        let project = directory.path().join("locked");
        std::fs::create_dir_all(&project)?;
        let config = project.join("micromux.yaml");
        std::fs::write(&config, "")?;
        let (_ports, lease) =
            registry.acquire(&config, "locked", &names(&["A"]), &[], &DEFAULT_PORT_RANGE)?;
        drop(lease);
        let record = directory
            .path()
            .join("registry")
            .join(format!("{}.json", config_path_key(&config)));

        std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o000))?;
        // Root ignores the permission, so the premise only holds for ordinary users.
        let premise = config.try_exists().is_err();
        let result = registry.acquire(
            Path::new("/projects/two/micromux.yaml"),
            "two",
            &[],
            &[],
            &DEFAULT_PORT_RANGE,
        );
        std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o755))?;
        result?;

        if premise {
            assert!(record.exists());
        }
        Ok(())
    }

    /// Excluded ports are never assigned, and a persisted assignment that became reserved is an
    /// error instead of a silent change.
    #[test]
    fn excluded_ports_are_never_assigned() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path());
        let config = Path::new("/projects/one/micromux.yaml");
        let (ports, lease) =
            registry.acquire(config, "one", &names(&["A"]), &[], &DEFAULT_PORT_RANGE)?;
        drop(lease);
        let port = *ports.get("A").ok_or_else(|| eyre::eyre!("no port"))?;

        let result = registry.acquire(config, "one", &names(&["A"]), &[port], &DEFAULT_PORT_RANGE);
        assert!(matches!(result, Err(SessionError::PortReserved { .. })));

        let other = Path::new("/projects/two/micromux.yaml");
        let (fresh, _lease) =
            registry.acquire(other, "two", &names(&["A"]), &[port], &DEFAULT_PORT_RANGE)?;
        assert_ne!(fresh.get("A"), Some(&port));
        Ok(())
    }

    /// Validation reads the persisted assignment without taking the session's lock.
    #[test]
    fn persisted_assignment_is_readable_while_live() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path());
        let config = Path::new("/projects/one/micromux.yaml");
        let (ports, _lease) =
            registry.acquire(config, "one", &names(&["A"]), &[], &DEFAULT_PORT_RANGE)?;

        assert_eq!(registry.persisted(config), Some(ports));
        Ok(())
    }

    /// A live session whose record cannot be read has unknown ports, so allocation refuses to
    /// guess.
    #[test]
    fn unreadable_record_of_a_live_session_is_an_error() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path());
        let first = Path::new("/projects/one/micromux.yaml");
        let (_ports, _lease) =
            registry.acquire(first, "one", &names(&["A"]), &[], &DEFAULT_PORT_RANGE)?;
        std::fs::write(
            directory
                .path()
                .join(format!("{}.json", config_path_key(first))),
            "{",
        )?;

        let result = registry.acquire(
            Path::new("/projects/two/micromux.yaml"),
            "two",
            &names(&["A"]),
            &[],
            &DEFAULT_PORT_RANGE,
        );

        assert!(matches!(result, Err(SessionError::UnreadableRecord { .. })));
        Ok(())
    }
}
