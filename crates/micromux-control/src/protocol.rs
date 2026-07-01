//! The control wire protocol: newline-delimited JSON request/response envelopes.
//!
//! Domain payloads (`ServiceSnapshot`, `HealthAttempt`, `LogLine`, `SessionChange`,
//! `ServiceCommandAck`) are the stable core types reused directly — no DTO mirror. The session and
//! the proxy are expected to be the same installed binary version; [`SessionInfo`] carries
//! [`PROTOCOL_VERSION`] so a mismatch fails loudly rather than weirdly.

use micromux::{
    HealthAttempt, LogLine, LogRunSummary, ServiceCommandAck, ServiceID, ServiceSnapshot,
    SessionChange,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The control protocol version. Bumped on any envelope change. The session and proxy are expected
/// to be from the same build; a mismatch is a hard error, not a negotiation.
pub const PROTOCOL_VERSION: u32 = 4;

/// A request from a client (the `micromux ctl` CLI or the MCP proxy) to a session's control server.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum Request {
    /// Return session identity and metadata.
    Describe,
    /// List every service with its current snapshot.
    ListServices,
    /// Return recent log records for a service.
    GetLogs {
        /// Target service.
        service: ServiceID,
        /// Specific disk-retained run generation to read. Omit to read the bounded visible log
        /// stream.
        run_generation: Option<u64>,
        /// Bound the result to the most recent records.
        tail: Option<usize>,
    },
    /// Return log records strictly after a monotonic cursor, for gap-free incremental following.
    FollowLogs {
        /// Target service.
        service: ServiceID,
        /// Specific disk-retained run generation to follow. Omit to follow the bounded visible log
        /// stream.
        run_generation: Option<u64>,
        /// Return only records with `seq` greater than this cursor (all retained records if
        /// `None`).
        after: Option<u64>,
    },
    /// Return summaries of retained log runs for a service.
    ListLogRuns {
        /// Target service.
        service: ServiceID,
    },
    /// Return the latest healthcheck attempt for a service.
    GetHealth {
        /// Target service.
        service: ServiceID,
    },
    /// Restart a single service.
    Restart {
        /// Target service.
        service: ServiceID,
    },
    /// Restart all enabled services.
    RestartAll,
    /// Enable (and start) a single service.
    Enable {
        /// Target service.
        service: ServiceID,
    },
    /// Disable a single service.
    Disable {
        /// Target service.
        service: ServiceID,
    },
    /// Stop the whole session: stop every service and exit the session process (graceful, like the
    /// operator pressing Ctrl-C), freeing its ports. Acknowledged with [`Response::ShuttingDown`]
    /// just before the endpoint goes away.
    Shutdown,
    /// Stream [`SessionChange`] notifications until the client disconnects.
    Subscribe,
}

/// A brief, identity-only view of a service for [`SessionInfo`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ServiceBrief {
    /// Stable service identifier.
    pub id: ServiceID,
    /// Human-readable service name.
    pub name: String,
}

/// Live session identity returned by [`Request::Describe`]. Never stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionInfo {
    /// The protocol version this session speaks.
    pub protocol_version: u32,
    /// Deterministic session id: the endpoint hash of the canonical config path.
    pub id: String,
    /// Process id of the session.
    pub pid: u32,
    /// Monotonic session start token as Unix nanoseconds. With `pid`, forms a start token.
    pub start_time: u64,
    /// Session name (config `name:` if set, else `basename(working_dir)`).
    pub name: String,
    /// The directory the session was launched in.
    pub working_dir: String,
    /// The canonical config path that keys this session's endpoint.
    pub config_path: String,
    /// The services this session supervises.
    pub services: Vec<ServiceBrief>,
    /// The micromux version of the session binary.
    pub micromux_version: String,
}

impl SessionInfo {
    /// Return whether another `Describe` response names the same running session instance.
    #[must_use]
    pub fn is_same_instance(&self, other: &Self) -> bool {
        self.id == other.id && self.pid == other.pid && self.start_time == other.start_time
    }
}

/// A typed error code shared by the protocol and the proxy diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ErrorCode {
    /// No service with the given id exists.
    UnknownService,
    /// The requested service run is no longer retained or never existed.
    UnknownRun,
    /// No session answered the request.
    NoSession,
    /// More than one live session matched the selector.
    Ambiguous,
    /// A live session is busy (connect/reply timeout); never a reason to delete it.
    Busy,
    /// A request timed out.
    Timeout,
    /// The command is invalid in the service's current state.
    InvalidState,
    /// The latest config could not be reloaded before a restart/enable command.
    ConfigReload,
    /// The scheduler stopped before acknowledging a command.
    SchedulerStopped,
    /// The control plane is not supported on this platform.
    UnsupportedPlatform,
    /// The peer speaks an incompatible protocol version.
    ProtocolVersionMismatch,
    /// The request could not be parsed.
    BadRequest,
    /// An unexpected internal error.
    Internal,
}

/// A response from a session's control server.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub enum Response {
    /// Reply to [`Request::Describe`].
    Description(SessionInfo),
    /// Reply to [`Request::ListServices`].
    Services(Vec<ServiceSnapshot>),
    /// Reply to [`Request::GetLogs`].
    Logs {
        /// The recent log records.
        lines: Vec<LogLine>,
        /// Whether the session had to drop older records to respect server response limits.
        truncated: bool,
    },
    /// Reply to [`Request::ListLogRuns`].
    LogRuns {
        /// Retained runs, oldest first.
        runs: Vec<LogRunSummary>,
    },
    /// Reply to [`Request::GetHealth`].
    Health(Option<HealthAttempt>),
    /// A mutation was *accepted* (validated + queued), not necessarily completed. Carries each
    /// affected service's latched generation.
    Accepted {
        /// Per-service acknowledgements.
        services: Vec<ServiceCommandAck>,
    },
    /// A streamed change notification (only after [`Request::Subscribe`]).
    Change(SessionChange),
    /// Acknowledgement of [`Request::Shutdown`], written just before the session begins exiting.
    ShuttingDown,
    /// A typed error.
    Error {
        /// The error code.
        code: ErrorCode,
        /// A human-readable message.
        message: String,
    },
}

impl Response {
    /// Build an [`Response::Error`] from a code and message.
    #[must_use]
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            code,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn session(id: &str, pid: u32, start_time: u64, name: &str) -> SessionInfo {
        SessionInfo {
            protocol_version: PROTOCOL_VERSION,
            id: id.to_string(),
            pid,
            start_time,
            name: name.to_string(),
            working_dir: ".".to_string(),
            config_path: "/project/micromux.yaml".to_string(),
            services: Vec::new(),
            micromux_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    #[test]
    fn session_instance_identity_uses_stable_start_token() {
        let first = session("aaa", 42, 100, "app");
        let alias = session("aaa", 42, 100, "renamed");
        let replacement = session("aaa", 42, 101, "app");

        assert!(first.is_same_instance(&alias));
        assert!(!first.is_same_instance(&replacement));
    }
}
