//! Binding the proxy's HTTP listeners.
//!
//! The proxy must answer on both `127.0.0.1` and `[::1]`: Linux resolves `*.localhost` to `::1`
//! first, and a free `127.0.0.1:80` next to someone else's `[::1]:80` would send part of the
//! traffic to that other program.
//! So the listeners are acquired as one set, and a port is only used when every address family
//! that is available could be bound.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};

use socket2::{Domain, Protocol, Socket, Type};

/// The port the proxy should listen on, and what to use when it cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortPreference {
    /// Preferred port, 80 by default.
    pub port: u16,
    /// Port used when `port` is privileged or taken; never chosen silently otherwise.
    pub fallback_port: Option<u16>,
}

impl PortPreference {
    /// Whether `port` is one of the acceptable ports.
    #[must_use]
    pub fn accepts(self, port: u16) -> bool {
        port == self.port || Some(port) == self.fallback_port
    }
}

/// Why the proxy cannot listen.
#[derive(Debug, thiserror::Error)]
pub enum BindError {
    /// The port needs privileges this process does not have.
    #[error(
        "cannot listen on port {port}: binding a port below 1024 needs privileges. {}",
        privileged_port_help()
    )]
    PermissionDenied {
        /// The refused port.
        port: u16,
    },
    /// Another program already listens on the port.
    #[error(
        "cannot listen on port {port}: another program already uses it. Stop that program or set \
         `proxy.fallback_port` in micromux.yaml"
    )]
    InUse {
        /// The taken port.
        port: u16,
    },
    /// Binding failed for another reason.
    #[error("cannot listen on {address}: {source}")]
    Io {
        /// The address that could not be bound.
        address: SocketAddr,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
}

#[cfg(target_os = "linux")]
fn privileged_port_help() -> &'static str {
    "Allow it once with `sudo sysctl -w net.ipv4.ip_unprivileged_port_start=80` (persist it in \
     /etc/sysctl.d/), grant micromux the capability with `sudo setcap \
     cap_net_bind_service=+ep \"$(command -v micromux)\"`, or set `proxy.fallback_port` (such \
     as 8080) in micromux.yaml"
}

#[cfg(not(target_os = "linux"))]
fn privileged_port_help() -> &'static str {
    "Set `proxy.fallback_port` (such as 8080) in micromux.yaml"
}

/// The listeners of one port.
#[derive(Debug)]
pub(crate) struct Bound {
    /// The port every listener uses.
    pub(crate) port: u16,
    /// One listener per available address family.
    pub(crate) listeners: Vec<TcpListener>,
    /// Whether the listeners are on the wildcard address, so connections from other hosts must
    /// be dropped at accept.
    pub(crate) wildcard: bool,
}

/// Binds the preferred port, or the fallback when the preferred one is privileged or taken.
pub(crate) fn bind(preference: PortPreference) -> Result<Bound, BindError> {
    match bind_port(preference.port) {
        Err(err @ (BindError::PermissionDenied { .. } | BindError::InUse { .. })) => {
            let Some(fallback) = preference.fallback_port else {
                return Err(err);
            };
            tracing::warn!(%err, fallback, "using the configured fallback port");
            bind_port(fallback)
        }
        result => result,
    }
}

fn bind_port(port: u16) -> Result<Bound, BindError> {
    match bind_set(port, false) {
        // macOS lets unprivileged processes bind low ports only on the wildcard address.
        Err(BindError::PermissionDenied { .. }) if cfg!(target_os = "macos") => {
            bind_set(port, true)
        }
        result => result,
    }
}

fn bind_set(port: u16, wildcard: bool) -> Result<Bound, BindError> {
    let (ipv4, ipv6) = if wildcard {
        (Ipv4Addr::UNSPECIFIED, Ipv6Addr::UNSPECIFIED)
    } else {
        (Ipv4Addr::LOCALHOST, Ipv6Addr::LOCALHOST)
    };
    let ipv4 = bind_one(SocketAddr::from((ipv4, port)))?;
    // Port 0 picks a free port for IPv4; IPv6 has to use the same one.
    let port = ipv4
        .local_addr()
        .map_err(|source| BindError::Io {
            address: SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            source,
        })?
        .port();
    let mut listeners = vec![ipv4];
    match bind_one(SocketAddr::from((ipv6, port))) {
        Ok(listener) => listeners.push(listener),
        Err(BindError::Io { source, .. }) if ipv6_unavailable(&source) => {
            tracing::warn!(%source, "IPv6 is unavailable; listening on IPv4 only");
        }
        Err(err) => return Err(err),
    }
    Ok(Bound {
        port,
        listeners,
        wildcard,
    })
}

fn bind_one(address: SocketAddr) -> Result<TcpListener, BindError> {
    let classify = |source: std::io::Error| match source.kind() {
        std::io::ErrorKind::PermissionDenied => BindError::PermissionDenied {
            port: address.port(),
        },
        std::io::ErrorKind::AddrInUse => BindError::InUse {
            port: address.port(),
        },
        _ => BindError::Io { address, source },
    };
    let socket = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(Protocol::TCP),
    )
    .map_err(classify)?;
    if address.is_ipv6() {
        // Keep the IPv6 socket from also claiming the IPv4 port, which the IPv4 listener owns.
        socket.set_only_v6(true).map_err(classify)?;
    }
    socket.set_reuse_address(true).map_err(classify)?;
    socket.set_nonblocking(true).map_err(classify)?;
    socket.bind(&address.into()).map_err(classify)?;
    socket.listen(1024).map_err(classify)?;
    Ok(socket.into())
}

/// Whether binding failed because the host has no usable IPv6 at all, rather than because the
/// address is taken.
fn ipv6_unavailable(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::AddrNotAvailable
        || err.raw_os_error().is_some_and(|code| {
            code == nix::errno::Errno::EAFNOSUPPORT as i32
                || code == nix::errno::Errno::EPROTONOSUPPORT as i32
        })
}

/// Whether a connection from `peer` may use a wildcard listener.
pub(crate) fn is_local_peer(peer: IpAddr) -> bool {
    peer.to_canonical().is_loopback()
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use similar_asserts::assert_eq;

    /// A taken preferred port moves the whole listener set to the fallback port.
    #[test]
    fn taken_port_uses_the_fallback_for_every_listener() -> eyre::Result<()> {
        let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let taken_port = taken.local_addr()?.port();
        let fallback = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?
            .local_addr()?
            .port();

        let bound = bind(PortPreference {
            port: taken_port,
            fallback_port: Some(fallback),
        })?;

        assert_eq!(bound.port, fallback);
        assert!(!bound.wildcard);
        for listener in &bound.listeners {
            assert_eq!(listener.local_addr()?.port(), fallback);
            assert!(listener.local_addr()?.ip().is_loopback());
        }
        Ok(())
    }

    /// Without a fallback a taken port is an error, never a silent switch to another port.
    #[test]
    fn taken_port_without_fallback_fails() -> eyre::Result<()> {
        let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = taken.local_addr()?.port();

        let result = bind(PortPreference {
            port,
            fallback_port: None,
        });

        assert!(matches!(result, Err(BindError::InUse { port: busy }) if busy == port));
        Ok(())
    }

    /// Someone else's `[::1]` listener makes the port unusable even though `127.0.0.1` is free,
    /// because browsers would reach that other program over IPv6.
    #[test]
    fn taken_ipv6_loopback_fails_the_whole_set() -> eyre::Result<()> {
        let Ok(taken) = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) else {
            // No IPv6 on this host: the rule cannot apply.
            return Ok(());
        };
        let port = taken.local_addr()?.port();
        if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_err() {
            return Ok(());
        }

        let result = bind(PortPreference {
            port,
            fallback_port: None,
        });

        assert!(matches!(result, Err(BindError::InUse { .. })), "{result:?}");
        Ok(())
    }

    #[test]
    fn only_loopback_peers_are_local() {
        assert!(is_local_peer(IpAddr::from(Ipv4Addr::LOCALHOST)));
        assert!(is_local_peer(IpAddr::from(Ipv6Addr::LOCALHOST)));
        assert!(is_local_peer(IpAddr::from(
            Ipv4Addr::LOCALHOST.to_ipv6_mapped()
        )));
        assert!(!is_local_peer(IpAddr::from(Ipv4Addr::new(192, 168, 1, 2))));
    }
}
