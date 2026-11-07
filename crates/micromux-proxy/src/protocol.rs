//! The registration protocol between sessions and the proxy: newline-delimited JSON over the
//! proxy's Unix socket.
//!
//! A connection either asks for [`ProxyMessage::Info`] with [`ClientMessage::Hello`], or
//! registers a session with [`ClientMessage::Register`] and then keeps the connection open,
//! sending [`ClientMessage::Ping`] at least every [`LEASE_TIMEOUT`].
//! A registration lives exactly as long as its connection: the proxy drops the session's routes
//! when the connection closes or falls silent for longer than the lease.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How often a registered session pings the proxy.
pub const PING_INTERVAL: Duration = Duration::from_secs(5);

/// How long the proxy waits for any message on a registered connection before it drops the
/// registration and closes the connection.
pub const LEASE_TIMEOUT: Duration = Duration::from_secs(15);

/// Longest accepted message line.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// A message from a session to the proxy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Asks which port the proxy listens on.
    Hello,
    /// Registers (or replaces) the routes of one session for the lifetime of this connection.
    ///
    /// Sent again on the same connection for the same session, it swaps the routes in place and
    /// keeps the session's proxied connections and tunnels open.
    Register(Registration),
    /// Keeps the registration alive.
    Ping,
}

/// A message from the proxy to a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProxyMessage {
    /// Reply to [`ClientMessage::Hello`].
    Info {
        /// The HTTP port the proxy listens on.
        port: u16,
        /// Process id of the proxy.
        pid: u32,
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
    /// The proxy port the session's services were told about; a proxy on another port refuses
    /// the registration.
    pub proxy_port: u16,
    /// The session's routes.
    pub routes: Vec<RouteTarget>,
}

/// One route of a registered session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteTarget {
    /// Hostname labels below the session domain, or `*` for the catch-all.
    pub host: String,
    /// Upstream port on 127.0.0.1.
    pub port: u16,
    /// The service listening on the port, named on error pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}
