//! The registration protocol between sessions and the proxy: newline-delimited JSON over the
//! proxy's Unix socket.
//!
//! A connection either asks for [`ProxyMessage::Info`] with [`ClientMessage::Hello`], or
//! registers a session with [`ClientMessage::Register`] and then keeps the connection open,
//! sending [`ClientMessage::Ping`] at least every [`LEASE_TIMEOUT`].
//! A registration lives exactly as long as its connection: the proxy drops the session's routes
//! when the connection closes or falls silent for longer than the lease.
//!
//! Every proxy accepts registrations from older sessions, whose messages lack the newer optional
//! fields.
//! An older proxy ignores fields it does not know, so a session whose routes need a newer
//! [`PROTOCOL_VERSION`] (see [`Registration::required_protocol`]) asks for
//! [`ProxyMessage::Info`] on its registration connection first and refuses a proxy that is too
//! old, rather than registering routes that proxy would serve differently.
//!
//! An outdated proxy that no session uses is only waiting out its [`IDLE_TIMEOUT`], so the
//! session takes it over instead: it asks the proxy to exit with [`ClientMessage::StopIfIdle`]
//! when [`ProxyMessage::Info`] lists the registered sessions, and otherwise waits for it to exit.
//! A proxy older than revision 3 lists its sessions only on the HTML page it answers a host no
//! session serves with, which the session reads instead.
//!
//! One proxy serves every port its sessions ask for.
//! A session learns its port from [`ClientMessage::Listen`] before its services start, and its
//! registration names that port in [`Registration::proxy_port`].
//! The proxy routes a request only to the sessions registered on the port it arrived on, and
//! stops listening on a port once no session has used it for [`IDLE_TIMEOUT`].
//! A proxy older than revision 4 listens on the one port it started with.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How often a registered session pings the proxy.
pub const PING_INTERVAL: Duration = Duration::from_secs(5);

/// How long the proxy waits for any message on a registered connection before it drops the
/// registration and closes the connection.
pub const LEASE_TIMEOUT: Duration = Duration::from_secs(15);

/// Longest accepted message line.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// How long a proxy started by a session keeps running without any registered session.
///
/// Every revision uses it, so it also bounds how long an outdated proxy that no session uses
/// keeps running.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The registration protocol this proxy and session speak.
///
/// Revisions:
///
/// - 1: hostname routes; a proxy that does not report a protocol speaks 1.
/// - 2: path routes, with [`RouteTarget::path`] and [`RouteTarget::strip_prefix`].
/// - 3: [`ProxyMessage::Info`] lists the registered sessions, and [`ClientMessage::StopIfIdle`]
///   stops a proxy that no session uses.
/// - 4: [`ClientMessage::Listen`] opens another port, and a registration opens the port it names.
pub const PROTOCOL_VERSION: u32 = 4;

/// The first protocol revision that routes by path.
pub const PATH_ROUTES_PROTOCOL: u32 = 2;

/// The first protocol revision that listens on more than one port.
pub const PORTS_PROTOCOL: u32 = 4;

fn first_protocol() -> u32 {
    1
}

/// A message from a session to the proxy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Asks which port the proxy started with, and which protocol it speaks.
    Hello,
    /// Asks the proxy to listen on `port`, or on `fallback_port` when `port` is privileged or
    /// taken, answered with [`ProxyMessage::Listening`].
    ///
    /// A port the proxy already listens on is answered right away.
    /// A port it opens stays open for at least [`IDLE_TIMEOUT`], which leaves the session time to
    /// register on it; it closes after no session has been registered on it for that long.
    /// A proxy older than revision 4 rejects it as an invalid message.
    Listen {
        /// Preferred port.
        port: u16,
        /// Port used when `port` is privileged or taken.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fallback_port: Option<u16>,
    },
    /// Registers (or replaces) the routes of one session for the lifetime of this connection.
    ///
    /// Sent again on the same connection for the same session, it swaps the routes in place and
    /// keeps the session's proxied connections and tunnels open.
    Register(Registration),
    /// Keeps the registration alive.
    Ping,
    /// Asks the proxy to exit now if no session is registered, answered with
    /// [`ProxyMessage::Stopping`] or [`ProxyMessage::Busy`].
    ///
    /// The proxy decides under the same lock as every registration, so a session that registers
    /// concurrently either keeps the proxy running or is refused as the proxy shuts down.
    /// A proxy older than revision 3 rejects it as an invalid message.
    StopIfIdle,
}

/// A message from the proxy to a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProxyMessage {
    /// Reply to [`ClientMessage::Hello`].
    Info {
        /// The HTTP port the proxy started with.
        ///
        /// From revision 4 on, the proxy may listen on other ports as well, and may have stopped
        /// listening on this one while no session used it.
        port: u16,
        /// Process id of the proxy.
        pid: u32,
        /// The registration protocol the proxy speaks.
        #[serde(default = "first_protocol")]
        protocol: u32,
        /// The registered sessions, or `None` from a proxy older than revision 3, which does
        /// not tell.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sessions: Option<Vec<RegisteredSession>>,
    },
    /// Reply to [`ClientMessage::Listen`]: the proxy listens on `port`.
    Listening {
        /// The port the session's routes are served on.
        port: u16,
    },
    /// The registration is active.
    Registered,
    /// The registration was refused; the proxy closes the connection after sending this.
    Rejected {
        /// Why, for the session to show.
        reason: String,
        /// Whether trying again (against a proxy started afresh) can succeed, as when this proxy
        /// is shutting down, rather than a conflict with another session.
        #[serde(default)]
        transient: bool,
    },
    /// Reply to [`ClientMessage::Ping`].
    Pong,
    /// Reply to [`ClientMessage::StopIfIdle`]: the proxy refuses every later registration and
    /// exits.
    Stopping,
    /// Reply to [`ClientMessage::StopIfIdle`]: sessions are registered, so the proxy keeps
    /// running.
    Busy {
        /// The registered sessions.
        sessions: Vec<RegisteredSession>,
    },
}

/// A session registered with the proxy, as the proxy reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredSession {
    /// Session name, such as `luup2`.
    pub session: String,
    /// Canonical config path of the session.
    pub config_path: String,
}

impl std::fmt::Display for RegisteredSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.session, self.config_path)
    }
}

/// The routes of one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    /// Session name, such as `luup2`.
    pub session: String,
    /// Session domain every route lives under, such as `luup2.localhost`.
    pub session_domain: String,
    /// Canonical config path, which identifies the session across reconnects.
    pub config_path: String,
    /// Process id of the session.
    pub pid: u32,
    /// The proxy port the session's services were told about, the only port its routes are
    /// served on.
    ///
    /// A proxy that does not listen on it yet opens it, without any fallback, and refuses the
    /// registration as retryable when it cannot; a proxy older than revision 4 refuses every
    /// port but its own that way.
    pub proxy_port: u16,
    /// The session's routes.
    pub routes: Vec<RouteTarget>,
}

impl Registration {
    /// The oldest protocol revision whose proxy serves these routes as registered.
    #[must_use]
    pub fn required_protocol(&self) -> u32 {
        if self.routes.iter().any(|route| route.path.is_some()) {
            PATH_ROUTES_PROTOCOL
        } else {
            first_protocol()
        }
    }
}

/// One route of a registered session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteTarget {
    /// Hostname labels below the session domain, or `*` for the catch-all.
    pub host: String,
    /// Path prefix the route serves, starting with `/`; `None` serves every path of the host.
    ///
    /// The prefix matches the request path as sent, before any percent-decoding, so `/rest/`
    /// matches `/rest/x` but not `/rest` or `/REST/x`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Whether the proxy removes `path` from the request path before forwarding; the forwarded
    /// path always starts with `/`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub strip_prefix: bool,
    /// Upstream port on 127.0.0.1.
    pub port: u16,
    /// The service listening on the port, named on error pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}
