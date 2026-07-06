//! The control wire protocol: newline-delimited JSON request/response envelopes.
//!
//! Domain payloads (`ServiceSnapshot`, `HealthAttempt`, `LogLine`, `SessionChange`,
//! `ServiceCommandAck`) are the stable core types reused directly — no DTO mirror. The session and
//! the proxy accept peers that speak the same major protocol version, so additive payload changes
//! do not orphan already-running sessions.

use micromux::{
    HealthAttempt, LogLine, LogRunSummary, ServiceCommandAck, ServiceID, ServiceSnapshot,
    SessionChange,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The current control protocol version.
///
/// Bump the minor for additive changes (new optional/defaulted fields, new tools that reuse
/// existing requests), and bump the major for incompatible request/response semantics.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::new(3, 0);

/// A typed control protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProtocolVersion {
    major: u16,
    minor: u16,
}

impl ProtocolVersion {
    /// Build a protocol version from major/minor components.
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    /// Major compatibility line.
    #[must_use]
    pub const fn major(self) -> u16 {
        self.major
    }

    /// Additive revision within one major line.
    #[must_use]
    pub const fn minor(self) -> u16 {
        self.minor
    }

    /// Return whether two peers can speak the same protocol.
    #[must_use]
    pub const fn is_compatible_with(self, peer: Self) -> bool {
        self.major == peer.major
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major(), self.minor())
    }
}

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
    /// Return the latest healthcheck attempt for a service's current live run.
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
    #[serde(default)]
    pub name: String,
}

/// Live session identity returned by [`Request::Describe`]. Never stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionInfo {
    /// The protocol version this session speaks.
    pub protocol_version: ProtocolVersion,
    /// Deterministic session id: the endpoint hash of the canonical config path.
    pub id: String,
    /// Process id of the session.
    pub pid: u32,
    /// Monotonic session start token as Unix nanoseconds. With `pid`, forms a start token.
    pub start_time: u64,
    /// Session name (config `name:` if set, else `basename(working_dir)`).
    #[serde(default)]
    pub name: String,
    /// The directory the session was launched in.
    #[serde(default)]
    pub working_dir: String,
    /// The canonical config path that keys this session's endpoint.
    #[serde(default)]
    pub config_path: String,
    /// The services this session supervises.
    #[serde(default)]
    pub services: Vec<ServiceBrief>,
    /// The micromux version of the session binary.
    #[serde(default)]
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
    /// A newer peer returned an error code this binary does not know yet.
    #[serde(other)]
    Unknown,
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
        #[serde(default)]
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
    use serde_json::json;
    use similar_asserts::assert_eq;

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

    #[test]
    fn protocol_version_uses_major_minor_shape_and_accepts_same_major() {
        assert_eq!(
            serde_json::to_value(PROTOCOL_VERSION).unwrap(),
            json!({ "major": 3, "minor": 0 })
        );
        assert_eq!(
            serde_json::from_value::<ProtocolVersion>(json!({ "major": 1, "minor": 0 })).unwrap(),
            ProtocolVersion::new(1, 0)
        );

        assert!(PROTOCOL_VERSION.is_compatible_with(ProtocolVersion::new(3, 9)));
        assert!(!PROTOCOL_VERSION.is_compatible_with(ProtocolVersion::new(2, 9)));
    }

    #[test]
    fn protocol_version_rejects_plain_integer_shape() {
        let err = serde_json::from_value::<ProtocolVersion>(json!(5)).unwrap_err();

        assert!(
            err.to_string().contains("invalid type"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn protocol_version_rejects_missing_minor() {
        let err = serde_json::from_value::<ProtocolVersion>(json!({ "major": 5 })).unwrap_err();

        assert!(
            err.to_string().contains("missing field `minor`"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn unknown_error_codes_degrade_without_deserialize_failure() {
        let code = serde_json::from_str::<ErrorCode>("\"FutureError\"").unwrap();

        assert_eq!(code, ErrorCode::Unknown);
    }

    #[test]
    fn session_info_accepts_missing_additive_fields() {
        let info = serde_json::from_value::<SessionInfo>(json!({
            "protocol_version": { "major": 3, "minor": 0 },
            "id": "abc",
            "pid": 42,
            "start_time": 99
        }))
        .unwrap();

        assert_eq!(info.protocol_version, PROTOCOL_VERSION);
        assert_eq!(info.name, "");
        assert!(info.services.is_empty());
        assert_eq!(info.micromux_version, "");
    }

    #[test]
    fn logs_response_accepts_missing_truncated_flag() {
        let response = serde_json::from_value::<Response>(json!({
            "Logs": {
                "lines": []
            }
        }))
        .unwrap();

        match response {
            Response::Logs { lines, truncated } => {
                assert!(lines.is_empty());
                assert!(!truncated);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }
}
