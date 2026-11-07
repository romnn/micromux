//! Ports that tests can hand from one proxy to the next without a parallel test taking them.
//!
//! A port the kernel picked for a test and the test released can be handed to any other process
//! right away.
//! These ports come from a small range below the ephemeral ranges of Linux (32768 and up) and
//! macOS (49152 and up), so no automatic bind or outgoing connection is ever given one, and a lock
//! file per port keeps parallel tests, in this process or another, off the ones in use.
//! The range also lies below the ports micromux allocates for sessions (20000 and up), which
//! other tests and running sessions take without these locks.

use std::fs::File;
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener};

/// The first port of the range.
const FIRST_PORT: u16 = 19_000;

/// How many ports the range holds; it bounds the lock files left in the temporary directory.
const PORTS: u16 = 200;

/// A port held for one test, until it is dropped.
#[derive(Debug)]
pub(crate) struct TestPort {
    /// The held port.
    pub(crate) port: u16,
    #[expect(
        dead_code,
        reason = "the lock on the port's file keeps parallel tests off the port while it is held"
    )]
    lock: File,
}

/// Holds a port of the range that no other test holds and nothing listens on.
///
/// # Errors
///
/// Fails when every port of the range is held or taken.
pub(crate) fn hold_port() -> std::io::Result<TestPort> {
    let offset = u16::try_from(std::process::id() % u32::from(PORTS)).unwrap_or_default();
    for index in 0..PORTS {
        let port = FIRST_PORT + (offset + index) % PORTS;
        let path = std::env::temp_dir().join(format!("micromux-test-port-{port}.lock"));
        let Ok(lock) = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
        else {
            continue;
        };
        if lock.try_lock().is_err() {
            continue;
        }
        if is_free(port) {
            return Ok(TestPort { port, lock });
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AddrInUse,
        format!("every test port from {FIRST_PORT} on is held or taken"),
    ))
}

/// Whether nothing listens on `port`.
///
/// The wildcard addresses count too: on macOS a loopback bind succeeds next to another program's
/// wildcard listener, which the proxy then finds and refuses.
fn is_free(port: u16) -> bool {
    // Each listener goes away before the next bind, so the IPv6 wildcard, which also covers
    // IPv4, does not collide with the IPv4 one.
    [Ipv4Addr::LOCALHOST, Ipv4Addr::UNSPECIFIED]
        .into_iter()
        .all(|address| TcpListener::bind((address, port)).is_ok())
        && [Ipv6Addr::LOCALHOST, Ipv6Addr::UNSPECIFIED]
            .into_iter()
            .all(|address| match TcpListener::bind((address, port)) {
                Ok(_) => true,
                // A host without IPv6 lets the proxy listen on IPv4 alone.
                Err(err) => ipv6_unavailable(&err),
            })
}

/// Whether binding failed because the host has no usable IPv6 at all.
fn ipv6_unavailable(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::AddrNotAvailable
        || err.raw_os_error().is_some_and(|code| {
            code == nix::errno::Errno::EAFNOSUPPORT as i32
                || code == nix::errno::Errno::EPROTONOSUPPORT as i32
        })
}
