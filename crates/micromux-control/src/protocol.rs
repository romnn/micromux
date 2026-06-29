//! The control wire protocol: newline-delimited JSON request/response envelopes.
//!
//! Domain payloads (`ServiceSnapshot`, `HealthAttempt`, `LogLine`, `SessionChange`,
//! `ServiceCommandAck`) are the stable core types reused directly — no DTO mirror. The session and
//! the proxy are expected to be the same installed binary version; [`SessionInfo`] carries
//! [`PROTOCOL_VERSION`] so a mismatch fails loudly rather than weirdly.

use micromux::{
    HealthAttempt, LogLine, ServiceCommandAck, ServiceID, ServiceSnapshot, SessionChange,
};
use serde::{Deserialize, Serialize};

/// The control protocol version. Bumped on any breaking envelope change. There is no cross-version
/// compatibility guarantee pre-1.0; a mismatch is a hard error, not a negotiation.
pub const PROTOCOL_VERSION: u32 = 1;

/// A request from a client (the `micromux ctl` CLI or the MCP proxy) to a session's control server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Return session identity and metadata.
    Describe,
    /// List every service with its current snapshot.
    ListServices,
    /// Return recent log lines for a service.
    GetLogs {
        /// Target service.
        service: ServiceID,
        /// Bound the result to the most recent lines.
        tail: Option<usize>,
    },
    /// Return log lines strictly after a monotonic cursor, for gap-free incremental following.
    FollowLogs {
        /// Target service.
        service: ServiceID,
        /// Return only lines with `seq` greater than this cursor (all retained lines if `None`).
        after: Option<u64>,
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
    /// Stream [`SessionChange`] notifications until the client disconnects.
    Subscribe,
}

/// A brief, identity-only view of a service for [`SessionInfo`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceBrief {
    /// Stable service identifier.
    pub id: ServiceID,
    /// Human-readable service name.
    pub name: String,
}

/// Live session identity returned by [`Request::Describe`]. Never stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    /// The protocol version this session speaks.
    pub protocol_version: u32,
    /// Deterministic session id: the endpoint hash of the canonical config path.
    pub id: String,
    /// Process id of the session.
    pub pid: u32,
    /// Session start time as a Unix timestamp (seconds). With `pid`, forms a start token.
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

/// A typed error code shared by the protocol and the proxy diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    /// No service with the given id exists.
    UnknownService,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    /// Reply to [`Request::Describe`].
    Description(SessionInfo),
    /// Reply to [`Request::ListServices`].
    Services(Vec<ServiceSnapshot>),
    /// Reply to [`Request::GetLogs`].
    Logs {
        /// The recent log lines.
        lines: Vec<LogLine>,
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
