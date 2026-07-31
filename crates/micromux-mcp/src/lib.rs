//! The micromux MCP server.
//!
//! A thin, near-stateless proxy: it discovers running micromux sessions over their local control
//! endpoints and exposes them as MCP tools. It holds no supervision state — every tool connects to
//! a session endpoint per call (cheap, local) and speaks the [`micromux_control`] protocol. All
//! actions go through the same control plane the human uses in the TUI, so dependency gating, health
//! re-probing, and restart policy are respected.
//!
//! The one tool that does more than connect is `start_session`: it spawns a detached, headless
//! `micromux serve` for a project that has no live session. `stop_session` is its inverse — it asks
//! a session to exit (freeing its ports), useful when switching between git worktrees that bind the
//! same ports.

mod convert;
mod logproc;
mod select;
mod tools;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use micromux::{Desired, Execution, Health, HealthAttempt, ServiceSnapshot};
use micromux_control::{
    Client, ControlEndpoint, ControlError, ErrorCode, Request, Response, SessionInfo, endpoint_for,
    runtime_dir_statuses, transport_supported, usable_runtime_dirs,
};
use regex::{Regex, RegexBuilder};
use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    handler::server::{
        router::tool::ToolRouter,
        wrapper::{Json, Parameters},
    },
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::select::ToolError;
use crate::tools::events::{ServiceEventPage, SessionServiceEvent, merge_service_events};
use crate::tools::health::{
    ServiceDiagnosis, SnapshotWait, WaitConclusion, bounded_attempt, diagnose_service,
    health_history, latest_health_for_snapshot, retired_service_message, service_needs_diagnosis,
    service_snapshot, timeout_hint, wait_for_health, wait_for_snapshot,
};
use crate::tools::logs::{
    FollowGap, ServiceFollowGap, compact_logs_since, current_cursors, current_service_cursor,
    fetch_and_shape_tail, follow_all_current_logs, follow_gap, next_follow_cursor,
    truncate_wait_matches,
};
use crate::tools::sessions::{
    CHILD_EXIT_GRACE, START_READY_TIMEOUT, already_running_any, reachable_session_for_start,
    remove_captured_stderr, spawn_detached_serve, start_failure_message, start_session_report,
};
#[cfg(unix)]
use crate::tools::sessions::{ProcessExitWatch, STOP_CONFIRM_TIMEOUT};

type ToolResult<T> = Result<Json<T>, ErrorData>;

const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 60;
const MAX_WAIT_TIMEOUT_SECS: u64 = 600;
const DEFAULT_RESTART_LOG_LIMIT: usize = 100;
const MAX_RESTART_LOG_LIMIT: usize = 1000;
/// Default number of logical log entries returned by `get_logs` when the caller does not pass
/// `tail`.
const DEFAULT_LOG_TAIL: usize = 200;
/// Upper bound on entries fetched from the session per call; also the window scanned when a filter is
/// active, so `grep`/`min_level` can match against more than the returned count.
const MAX_LOG_TAIL: usize = 2000;
const DEFAULT_EVENT_TAIL: usize = 50;
const MAX_EVENT_TAIL: usize = micromux::EVENT_HISTORY;
const DEFAULT_DIAGNOSE_TAIL: usize = 5;
const MAX_DIAGNOSE_TAIL: usize = 20;
const DIAGNOSE_LOG_SCAN: usize = 500;
/// Even if a change notification is missed (the subscription isn't registered server-side until
/// after we connect, so it can drop the first one), re-poll the lossless model at least this often
/// so a quiet, healthcheck-less service that becomes healthy is never stranded until the full
/// timeout.
const WAIT_POLL_FLOOR: Duration = Duration::from_secs(1);
const WAIT_LOG_POLL: Duration = Duration::from_millis(250);
/// Optional cursor capture must not materially delay the mutation it annotates.
const MUTATION_CURSOR_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_SESSION_STARTS_PER_WINDOW: usize = 8;
const SESSION_START_WINDOW: Duration = Duration::from_mins(1);
const GREP_REGEX_SIZE_LIMIT: usize = 256 * 1024;
const GREP_PATTERN_MAX_BYTES: usize = 64 * 1024;

const INSTRUCTIONS: &str = "Discover and control running micromux sessions. When no `session` is \
given, tools target the current project's session. Use `find_service` to locate a service across \
sessions; service-scoped errors also point at matching sibling sessions. Validate a candidate file \
with `validate_config`; for an active session's on-disk edits, call `reconcile_config` with \
`dry_run=true` before applying it. Reconciliation adds, retires, and updates configured services but \
does not restart changed processes. Before creating a runtime service, use `list_sessions` and check \
`capabilities.dynamic_services`. Supply an `idempotency_key` so creation retries are safe. Leases \
are bounded by session policy unless it explicitly permits `expires_after=\"none\"`; use \
`renew_dynamic_service` to extend a live lease without restarting. For servers, use \
`start_dynamic_service_and_wait` for the create/wait/log/events/diagnosis bundle, or compose \
`start_dynamic_service`, `wait_for_healthy(after_generation=observed_generation)`, and \
`follow_logs`. Use `wait_for_exit` for one-shot jobs instead of waiting for health. \
`ensure_service_ready` can start a missing session, enable a disabled configured service, and wait \
for readiness in one call. Use `get_service_events` for startup forensics; pass `service=\"*\"` \
with its per-service cursor map to follow ordering across the session. `restart_service` and \
`enable_service` return a generation for `wait_for_healthy`; `restart_service_and_wait` bundles the \
usual cursor/restart/wait/log flow. Use `wait_for_log` after external actions, `diagnose` for a \
one-shot failure summary, `get_health_history` when a flapping probe needs more than the latest \
attempt, `list_log_runs` for retained runs, and `log_cursors` plus \
`follow_all_logs` around hot reloads. Log tools strip ANSI by default and support regex, context, \
time, trace-id, compact JSON, and minimum-level filters. Actions go through micromux and retain its \
dependency and restart semantics. Use `start_session` to create a project's headless session; a \
human can run `micromux attach` to observe it. Use `stop_session` to stop it explicitly.";

/// The MCP server handler. Cheap to clone; holds no supervision state.
#[derive(Clone)]
pub struct McpServer {
    cwd: PathBuf,
    allow_session_start: bool,
    session_start: Arc<tokio::sync::Semaphore>,
    session_start_budget: Arc<tokio::sync::Semaphore>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SessionArgs {
    /// Optional session selector: a session name, `name:<n>`, `pid:<n>`, `hash:<h>`, or omitted to
    /// target the micromux running in the current project directory.
    #[serde(default)]
    session: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReconcileConfigArgs {
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Compute and return the semantic diff without changing the session.
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ServiceArgs {
    /// The id of the target service.
    service: String,
    /// Optional session selector (see `list_sessions`); omit for the current project.
    #[serde(default)]
    session: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ServiceEventsArgs {
    /// The id of the target service, or `*` to merge events from every service.
    service: String,
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Return events with a sequence strictly greater than this cursor.
    #[serde(default)]
    after_seq: Option<u64>,
    /// Per-service cursors for `service="*"`; missing services start before their first event.
    #[serde(default)]
    after: Option<BTreeMap<String, u64>>,
    /// Maximum events to return (default 50, capped at 256).
    #[serde(default)]
    tail: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DynamicServiceArgs {
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Dynamic service definition, clone source, lease, and idempotency key.
    #[serde(flatten)]
    params: micromux::DynamicServiceParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReplaceDynamicServiceArgs {
    /// Revision currently observed by the caller.
    expected_revision: u64,
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Replacement definition, clone source, lease, and idempotency key.
    #[serde(flatten)]
    params: micromux::DynamicServiceParams,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RenewDynamicServiceArgs {
    /// Dynamic service id.
    service: String,
    /// Revision currently observed by the caller.
    expected_revision: u64,
    /// Replacement lease; omit to renew with the session policy default.
    #[serde(default)]
    expires_after: Option<micromux::Lease>,
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FindServiceArgs {
    /// The service id or human name to locate across every running session.
    service: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LogFilterArgs {
    /// Keep ANSI color escapes instead of stripping them. Default false (stripped) to save tokens.
    #[serde(default)]
    raw: bool,
    /// Keep only entries matching this regex (applied after ANSI stripping).
    #[serde(default)]
    #[schemars(length(max = 65536))]
    grep: Option<String>,
    /// Include this many neighboring entries before and after each grep match (like grep -C).
    /// Has no effect unless `grep` is set. Capped at 20.
    #[serde(default)]
    grep_context: Option<usize>,
    /// Keep only structured-JSON log entries at or above this level
    /// (`trace`<`debug`<`info`<`warn`<`error`<`fatal`). Entries that are not JSON with a level field
    /// are dropped when this is set, so only use it for services that emit JSON logs.
    #[serde(default)]
    min_level: Option<String>,
    /// Keep only entries at or after this time. Accepts RFC3339, Unix seconds/ms/us/ns, or relative
    /// durations like `30s`, `2m`, `1h`, `500ms`.
    #[serde(default)]
    since: Option<String>,
    /// Keep only entries at or after this Unix millisecond timestamp.
    #[serde(default)]
    since_unix_ms: Option<u64>,
    /// Keep only entries containing this trace/correlation id.
    #[serde(default)]
    trace_id: Option<String>,
    /// Output format. `compact` turns structured JSON records into token-efficient text plus typed
    /// `message`/`fields`; `full` returns the cleaned logical record.
    #[serde(default)]
    format: logproc::LogFormat,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LogsArgs {
    /// The id of the target service, or `*` to merge the current visible logs from all services.
    service: String,
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Optional run generation. Omit to read the bounded visible log stream; pass a retained run
    /// generation to read a bounded tail from that disk-backed run.
    #[serde(default)]
    run_generation: Option<u64>,
    /// Number of most recent logical log entries to return (default 200, capped at 2000).
    #[serde(default)]
    tail: Option<usize>,
    #[serde(flatten)]
    filters: LogFilterArgs,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FollowArgs {
    /// The id of the target service.
    service: String,
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Optional run generation. Omit to follow the bounded visible log stream; pass a retained run
    /// generation to page through that disk-backed run.
    #[serde(default)]
    run_generation: Option<u64>,
    /// Return only log entries after this cursor. Pass the `next_seq` from the previous call to
    /// resume without gaps or duplicates; pass 0 to start before the first entry. For the current
    /// visible stream, omit to return its latest tail; for a retained run, omit to start at the
    /// beginning.
    #[serde(default)]
    after_seq: Option<u64>,
    #[serde(flatten)]
    filters: LogFilterArgs,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FollowAllArgs {
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Per-service cursors from the previous `follow_all_logs` call. Cursor 0 means "before this
    /// service's first entry". Omit the whole map to return the latest merged tail; use
    /// `log_cursors` before an action to capture a "since now" cursor map.
    #[serde(default)]
    after: BTreeMap<String, u64>,
    /// Maximum merged entries to return (default 200, capped at 2000).
    #[serde(default)]
    limit: Option<usize>,
    #[serde(flatten)]
    filters: LogFilterArgs,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StartArgs {
    /// Project directory or config file to start a session for. A directory is searched upward for a
    /// micromux config; omit to use the MCP server's working directory.
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ValidateConfigArgs {
    /// Project directory or config file to validate. A directory is searched upward; omit to use
    /// the MCP server's working directory.
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EnsureReadyArgs {
    /// The id of the service to make ready.
    service: String,
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Project directory or config file to start if no session is running.
    #[serde(default)]
    path: Option<String>,
    /// Maximum seconds to wait (default 60, capped at 600).
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Maximum likely-cause log entries used by follow-up diagnosis (default 5, capped at 20).
    #[serde(default)]
    log_limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitArgs {
    /// The id of the target service.
    service: String,
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Resolve only once a run with generation greater than this exists. Pass the `generation`
    /// returned by `restart_service`/`enable_service` to wait for the new run, not the old one.
    #[serde(default)]
    after_generation: Option<u64>,
    /// Maximum seconds to wait (default 60, capped at 600).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitForExitArgs {
    /// The id of the target service.
    service: String,
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Resolve only once a run with generation greater than this exits.
    #[serde(default)]
    after_generation: Option<u64>,
    /// Maximum seconds to wait (default 60, capped at 600).
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Maximum compact log entries to return (default 100, capped at 1000).
    #[serde(default)]
    log_limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RestartAndWaitArgs {
    /// The id of the target service.
    service: String,
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Maximum seconds to wait (default 60, capped at 600).
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Maximum compact log entries to return (default 100, capped at 1000).
    #[serde(default)]
    log_limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StartDynamicAndWaitArgs {
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Dynamic service definition, clone source, lease, and idempotency key.
    #[serde(flatten)]
    params: micromux::DynamicServiceParams,
    /// Maximum seconds to wait (default 60, capped at 600).
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Maximum compact log entries to return (default 100, capped at 1000).
    #[serde(default)]
    log_limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DiagnoseArgs {
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Maximum likely-cause log entries per diagnosed service (default 5, capped at 20).
    #[serde(default)]
    tail_per_service: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitLogArgs {
    /// The id of the target service, or `*` to wait across all current visible service logs.
    service: String,
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Single-service cursor. Omit to start waiting from the current end of that service's visible
    /// log stream.
    #[serde(default)]
    after_seq: Option<u64>,
    /// Cross-service cursors from `log_cursors` or `follow_all_logs.next`. Used only when
    /// `service="*"`. Omit to start waiting from the current end of every service.
    #[serde(default)]
    after: BTreeMap<String, u64>,
    /// Maximum seconds to wait (default 60, capped at 600).
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Maximum matching entries to return once a match is found (default 20, capped at 2000).
    #[serde(default)]
    limit: Option<usize>,
    #[serde(flatten)]
    filters: LogFilterArgs,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LogFilePathArgs {
    /// The id of the target service.
    service: String,
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Run generation to resolve. Omit to return the current retained run's file path.
    #[serde(default)]
    run_generation: Option<u64>,
}

/// Common session identity flattened into every mutation receipt.
///
/// Receipts name two known generations `previous_generation` and `run_generation`; when
/// only the pre-action latch is known, they use `generation`.
#[derive(Serialize, JsonSchema)]
struct SessionRef {
    session: String,
    id: String,
    pid: u32,
    config_path: String,
}

impl From<&SessionInfo> for SessionRef {
    fn from(info: &SessionInfo) -> Self {
        Self {
            session: info.name.clone(),
            id: info.id.clone(),
            pid: info.pid,
            config_path: info.config_path.clone(),
        }
    }
}

#[derive(Serialize, JsonSchema)]
struct MutationResult {
    #[serde(flatten)]
    session_ref: SessionRef,
    accepted: Vec<micromux::ServiceCommandAck>,
    service: String,
    generation: Option<u64>,
    /// The service's last visible log seq, when it could be captured before the mutation was sent.
    ///
    /// Pass it to `follow_logs` as `after_seq` to read exactly the output this action caused.
    log_cursor: Option<u64>,
}

#[derive(Serialize, JsonSchema)]
struct DynamicMutationResult {
    #[serde(flatten)]
    session_ref: SessionRef,
    #[serde(flatten)]
    receipt: micromux_control::DynamicServiceAck,
}

struct DynamicMutation {
    resolved: select::Resolved,
    receipt: micromux_control::DynamicServiceAck,
}

impl DynamicMutation {
    fn into_result(self) -> DynamicMutationResult {
        DynamicMutationResult {
            session_ref: SessionRef::from(&self.resolved.info),
            receipt: self.receipt,
        }
    }
}

#[derive(Serialize, JsonSchema)]
struct RestartAndWaitResult {
    #[serde(flatten)]
    session_ref: SessionRef,
    service: String,
    previous_generation: u64,
    run_generation: u64,
    outcome: WaitResult,
    snapshot: ServiceSnapshot,
    logs: Vec<logproc::ProcessedEntry>,
    logs_truncated: bool,
    /// Lifecycle timeline events for the restart (from the pre-restart generation onward), so a
    /// failed wait explains itself (blocked dependency, backoff, spawn failure) without another call.
    events: Vec<micromux::ServiceEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnosis: Option<ServiceDiagnosis>,
}

#[derive(Serialize, JsonSchema)]
struct StartDynamicAndWaitResult {
    #[serde(flatten)]
    session_ref: SessionRef,
    #[serde(flatten)]
    receipt: micromux_control::DynamicServiceAck,
    outcome: WaitResult,
    snapshot: ServiceSnapshot,
    logs: Vec<logproc::ProcessedEntry>,
    logs_truncated: bool,
    events: Vec<micromux::ServiceEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnosis: Option<ServiceDiagnosis>,
}

#[derive(Serialize, JsonSchema)]
struct SessionListResult {
    sessions: Vec<SessionInfo>,
    diagnostics: select::DiscoveryDiagnostics,
}

#[derive(Serialize, JsonSchema)]
struct ServiceListResult {
    config_path: String,
    session: String,
    /// A copy-pasteable selector (`hash:<id>`) that resolves back to this exact session; pass it as
    /// `session` on follow-up calls to avoid re-disambiguating.
    session_selector: String,
    services: Vec<ServiceSnapshot>,
}

#[derive(Serialize, JsonSchema)]
struct FindServiceResult {
    service: String,
    matches: Vec<ServiceLocation>,
}

#[derive(Serialize, JsonSchema)]
struct ServiceLocation {
    /// The session's name.
    session: String,
    /// A copy-pasteable selector (`hash:<id>`) that targets this session.
    session_selector: String,
    /// The session's process id.
    pid: u32,
    /// The session's config path.
    config_path: String,
    /// The directory the session was launched in.
    working_dir: String,
    /// The matching service's current snapshot (state, health, run generation, command, advertised
    /// ports), or `None` if the session did not return a snapshot for it (it stopped answering,
    /// returned an error, or no longer supervises the service).
    snapshot: Option<ServiceSnapshot>,
}

#[derive(Serialize, JsonSchema)]
struct LogsResult {
    service: String,
    run_generation: Option<u64>,
    config_path: String,
    entries: Vec<logproc::ProcessedEntry>,
    truncated: bool,
}

#[derive(Serialize, JsonSchema)]
struct LogRunsResult {
    service: String,
    config_path: String,
    runs: Vec<micromux::LogRunSummary>,
}

#[derive(Serialize, JsonSchema)]
struct LogFilePathResult {
    service: String,
    config_path: String,
    run_generation: u64,
    path: String,
}

#[derive(Serialize, JsonSchema)]
struct SessionMutationResult {
    #[serde(flatten)]
    session_ref: SessionRef,
    accepted: Vec<micromux::ServiceCommandAck>,
}

#[derive(Serialize, JsonSchema)]
struct ReconcileConfigResult {
    #[serde(flatten)]
    session_ref: SessionRef,
    dry_run: bool,
    actions: Vec<micromux::ReconcileAction>,
}

#[derive(Serialize, JsonSchema)]
struct StopSessionResult {
    stopped: bool,
    #[serde(flatten)]
    session_ref: SessionRef,
    note: Option<&'static str>,
}

#[derive(Serialize, JsonSchema)]
struct StartSessionResult {
    started: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    already_running: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reachable: Option<bool>,
    config_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Serialize, JsonSchema)]
struct FollowLogsResult {
    service: String,
    run_generation: Option<u64>,
    entries: Vec<logproc::ProcessedEntry>,
    next_seq: Option<u64>,
    gap: Option<FollowGap>,
    truncated: bool,
}

#[derive(Serialize, JsonSchema)]
struct FollowAllLogsResult {
    config_path: String,
    entries: Vec<logproc::ProcessedEntry>,
    next: BTreeMap<String, u64>,
    gaps: Vec<ServiceFollowGap>,
    truncated: bool,
}

#[derive(Serialize, JsonSchema)]
struct WaitForLogResult {
    status: WaitForLogStatus,
    service: String,
    config_path: String,
    entries: Vec<logproc::ProcessedEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_seq: Option<u64>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    next: BTreeMap<String, u64>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    waited_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'static str>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WaitForLogStatus {
    Matched,
    Timeout,
}

#[derive(Serialize, JsonSchema)]
struct LogCursorsResult {
    config_path: String,
    cursors: BTreeMap<String, u64>,
}

#[derive(Serialize, JsonSchema)]
struct HealthResult {
    config_path: String,
    service: String,
    snapshot: ServiceSnapshot,
    latest_healthcheck: Option<HealthAttempt>,
}

#[derive(Serialize, JsonSchema)]
struct HealthHistoryResult {
    config_path: String,
    service: String,
    snapshot: ServiceSnapshot,
    /// Retained probe attempts for the current or latest run, oldest first.
    attempts: Vec<HealthAttempt>,
}

#[derive(Serialize, JsonSchema)]
struct SingleServiceEventsResult {
    #[serde(flatten)]
    session_ref: SessionRef,
    service: String,
    /// Lifecycle timeline events, oldest first.
    events: Vec<micromux::ServiceEvent>,
    /// Highest event seq returned; pass as `after_seq` to follow incrementally.
    next_seq: Option<u64>,
    /// Whether older events were dropped by the tail bound or ring eviction.
    truncated: bool,
}

#[derive(Serialize, JsonSchema)]
struct SessionServiceEventsResult {
    #[serde(flatten)]
    session_ref: SessionRef,
    service: String,
    /// Lifecycle timeline events from every service, oldest first.
    events: Vec<SessionServiceEvent>,
    /// Per-service cursors; pass this map as `after` to follow incrementally.
    next: BTreeMap<String, u64>,
    /// Whether older events were dropped by the shared tail bound or ring eviction.
    truncated: bool,
}

#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
enum ServiceEventsResult {
    Single(SingleServiceEventsResult),
    Session(SessionServiceEventsResult),
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum EnsureActionKind {
    StartedSession,
    EnabledService,
    Waited,
}

#[derive(Serialize, JsonSchema)]
struct EnsureAction {
    /// What the tool did on the caller's behalf.
    kind: EnsureActionKind,
    detail: String,
}

#[derive(Serialize, JsonSchema)]
struct EnsureServiceReadyResult {
    #[serde(flatten)]
    session_ref: SessionRef,
    service: String,
    /// Every state change this call performed, in order; empty when already ready.
    actions: Vec<EnsureAction>,
    outcome: WaitResult,
    snapshot: ServiceSnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnosis: Option<ServiceDiagnosis>,
}

#[derive(Serialize, JsonSchema)]
struct DiagnoseResult {
    config_path: String,
    /// The resolved session's name.
    session: String,
    /// A copy-pasteable selector (`hash:<id>`) for the diagnosed session.
    session_selector: String,
    /// Current disk-log read capacity, when advertised by the session.
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_log_reads: Option<micromux_control::DiskLogReadHealth>,
    services: Vec<ServiceDiagnosis>,
}

#[derive(Serialize, JsonSchema)]
#[serde(transparent)]
struct Nullable<T>(Option<T>);

#[derive(Serialize, JsonSchema)]
struct WaitResult {
    status: WaitStatus,
    service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<Nullable<i32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waited_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    execution: Option<Execution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    healthcheck_configured: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    health: Option<Nullable<Health>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uptime_secs: Option<Nullable<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_healthcheck: Option<Nullable<HealthAttempt>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'static str>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WaitStatus {
    Healthy,
    Exited,
    Timeout,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WaitForExitStatus {
    Exited,
    Timeout,
}

#[derive(Serialize, JsonSchema)]
struct WaitForExitResult {
    #[serde(flatten)]
    session_ref: SessionRef,
    service: String,
    status: WaitForExitStatus,
    exit_code: Option<i32>,
    run_generation: u64,
    snapshot: ServiceSnapshot,
    logs: Vec<logproc::ProcessedEntry>,
    /// Whether the log tail was cut by the limit; fetch the rest with `follow_logs`.
    logs_truncated: bool,
    waited_secs: u64,
}

enum ExitVerdict {
    Exited,
    InvalidState,
}

fn classify_exit(snapshot: &ServiceSnapshot, after_generation: Option<u64>) -> Option<ExitVerdict> {
    let generation_ready = convert::generation_ready(snapshot, after_generation);
    if generation_ready && snapshot.execution == Execution::Exited {
        return Some(ExitVerdict::Exited);
    }

    let retired = snapshot.retired.is_some();
    let unavailable = snapshot.desired == Desired::Disabled || retired;
    let qualifying_run_in_flight = generation_ready
        && matches!(
            snapshot.execution,
            Execution::Starting | Execution::Running | Execution::Stopping
        );
    (unavailable && !qualifying_run_in_flight).then_some(ExitVerdict::InvalidState)
}

impl WaitResult {
    fn from_conclusion(service: String, timeout: Duration, conclusion: &WaitConclusion) -> Self {
        match conclusion {
            WaitConclusion::Healthy(snapshot) => Self::healthy(service, snapshot.run_generation),
            WaitConclusion::Exited(snapshot) => {
                Self::exited(service, snapshot.last_exit_code, snapshot.run_generation)
            }
            WaitConclusion::Timeout {
                snapshot,
                latest_healthcheck,
            } => Self::timeout(
                service,
                timeout.as_secs(),
                snapshot,
                latest_healthcheck.clone(),
            ),
        }
    }

    fn healthy(service: String, generation: u64) -> Self {
        Self {
            status: WaitStatus::Healthy,
            service,
            generation: Some(generation),
            exit_code: None,
            waited_secs: None,
            execution: None,
            run_generation: None,
            healthcheck_configured: None,
            health: None,
            uptime_secs: None,
            latest_healthcheck: None,
            hint: None,
        }
    }

    fn exited(service: String, exit_code: Option<i32>, run_generation: u64) -> Self {
        Self {
            status: WaitStatus::Exited,
            service,
            generation: None,
            exit_code: Some(Nullable(exit_code)),
            waited_secs: None,
            execution: None,
            run_generation: Some(run_generation),
            healthcheck_configured: None,
            health: None,
            uptime_secs: None,
            latest_healthcheck: None,
            hint: Some("the run exited before becoming healthy — inspect get_logs for why"),
        }
    }

    fn timeout(
        service: String,
        waited_secs: u64,
        snapshot: &ServiceSnapshot,
        latest_healthcheck: Option<HealthAttempt>,
    ) -> Self {
        Self {
            status: WaitStatus::Timeout,
            service,
            generation: None,
            exit_code: None,
            waited_secs: Some(waited_secs),
            execution: Some(snapshot.execution),
            run_generation: Some(snapshot.run_generation),
            healthcheck_configured: Some(snapshot.healthcheck_configured),
            health: Some(Nullable(snapshot.health)),
            uptime_secs: Some(Nullable(snapshot.uptime.map(|uptime| uptime.as_secs()))),
            latest_healthcheck: Some(Nullable(latest_healthcheck)),
            hint: Some(timeout_hint(snapshot)),
        }
    }
}

async fn send_request(endpoint: &ControlEndpoint, request: Request) -> Result<Response, ToolError> {
    let mut client = Client::connect(endpoint).await?;
    Ok(client.request(request).await?)
}

struct SessionConn {
    endpoint: ControlEndpoint,
    client: Client,
}

impl SessionConn {
    async fn connect(endpoint: &ControlEndpoint) -> Result<Self, ToolError> {
        Ok(Self {
            endpoint: endpoint.clone(),
            client: Client::connect(endpoint).await?,
        })
    }

    async fn request(&mut self, request: Request) -> Result<Response, ToolError> {
        let retry_safe = request.is_retry_safe();
        match self.client.request(request.clone()).await {
            Ok(response) => Ok(response),
            Err(ControlError::Io(_) | ControlError::Closed) if retry_safe => {
                self.client = Client::connect(&self.endpoint).await?;
                Ok(self.client.request(request).await?)
            }
            Err(err) => Err(err.into()),
        }
    }
}

/// A copy-pasteable session selector (`hash:<id>`) that resolves back to this exact session.
fn session_selector(info: &SessionInfo) -> String {
    format!("hash:{}", info.id)
}

/// Whether `info` supervises a service matching `service` by stable id or human name. Used by
/// `find_service`, where discovery by human name is intended.
fn session_has_service(info: &SessionInfo, service: &str) -> bool {
    info.services
        .iter()
        .any(|brief| brief.id == service || brief.name == service)
}

fn matching_service(services: Vec<ServiceSnapshot>, selector: &str) -> Option<ServiceSnapshot> {
    let index = services
        .iter()
        .position(|snapshot| snapshot.id == selector)
        .or_else(|| {
            services
                .iter()
                .position(|snapshot| snapshot.name == selector)
        })?;
    services.into_iter().nth(index)
}

async fn service_in_session(resolved: &select::Resolved, service: &str) -> Option<ServiceSnapshot> {
    // Older peers do not understand the targeted request. Their unbounded description contained
    // the complete service index, so only use the legacy roster request after that index matches.
    if resolved.info.protocol_version.minor() >= 5 {
        if !resolved.info.services_truncated && !session_has_service(&resolved.info, service) {
            return None;
        }
        return send_request(
            &resolved.endpoint,
            Request::GetService {
                service: service.to_string(),
            },
        )
        .await
        .ok()
        .and_then(|response| convert::service(response).ok());
    }

    if !session_has_service(&resolved.info, service) {
        return None;
    }
    send_request(&resolved.endpoint, Request::ListServices)
        .await
        .ok()
        .and_then(|response| convert::services(response).ok())
        .and_then(|services| matching_service(services, service))
}

/// Enrich a bare `UnknownService` error with the sibling sessions that *do* resolve the service, so
/// the agent can retarget instead of guessing. Any other error passes through untouched. Best-effort:
/// a discovery failure leaves the error as-is rather than masking it.
async fn enrich_unknown_service(service: &str, mut err: ToolError) -> ToolError {
    let ToolError::Remote {
        code: ErrorCode::UnknownService,
        message,
    } = &mut err
    else {
        return err;
    };
    let siblings = select::answering_sessions()
        .await
        .unwrap_or_default()
        .into_iter();
    let mut matching_siblings = Vec::new();
    for resolved in siblings {
        if service_in_session(&resolved, service)
            .await
            .is_some_and(|snapshot| snapshot.id == service)
        {
            matching_siblings.push(resolved);
        }
    }
    let siblings = matching_siblings;
    if !siblings.is_empty() {
        let locations = siblings
            .iter()
            .map(|resolved| {
                format!(
                    "{} (config {})",
                    session_selector(&resolved.info),
                    resolved.info.config_path
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        *message = format!(
            "{message}. `{service}` was not found in the selected session but exists in {} other \
             answering session(s); retarget by passing one as `session`: {locations}",
            siblings.len()
        );
    }
    err
}

/// Map a single-service control result into a tool result, enriching an `UnknownService` error with
/// the sibling sessions that do have the service so the agent can retarget in one hop.
/// Recent lifecycle events for one service, bounded by the model's retention.
async fn service_events_tail(
    conn: &mut SessionConn,
    service: &str,
) -> Result<Vec<micromux::ServiceEvent>, ErrorData> {
    let response = conn
        .request(Request::GetEvents {
            service: service.to_string(),
            after: None,
            tail: Some(micromux::EVENT_HISTORY),
        })
        .await
        .map_err(error_data)?;
    let (events, _) = service_result(service, convert::events(response)).await?;
    Ok(events)
}

async fn service_result<T>(service: &str, result: Result<T, ToolError>) -> Result<T, ErrorData> {
    match result {
        Ok(value) => Ok(value),
        Err(err) => Err(error_data(enrich_unknown_service(service, err).await)),
    }
}

struct LogFilters {
    grep: Option<Regex>,
    grep_context: usize,
    min_level: Option<logproc::Level>,
    since_unix_ms: Option<u64>,
    trace_id: Option<String>,
    format: logproc::LogFormat,
}

impl LogFilters {
    fn from_args(args: &LogFilterArgs) -> Result<Self, ErrorData> {
        Ok(Self {
            grep: compile_grep(args.grep.as_deref())?,
            grep_context: args.grep_context.unwrap_or(0).min(20),
            min_level: parse_min_level(args.min_level.as_deref())?,
            since_unix_ms: parse_since(args.since.as_deref(), args.since_unix_ms)?,
            trace_id: args.trace_id.clone(),
            format: args.format,
        })
    }

    fn shape(&self, raw: bool, limit: Option<usize>) -> logproc::Shape<'_> {
        logproc::Shape {
            raw,
            grep: self.grep.as_ref(),
            context: self.grep_context,
            min_level: self.min_level,
            since_unix_ms: self.since_unix_ms,
            trace_id: self.trace_id.as_deref(),
            limit,
            format: self.format,
        }
    }

    fn filters_records(&self) -> bool {
        self.grep.is_some()
            || self.min_level.is_some()
            || self.since_unix_ms.is_some()
            || self.trace_id.is_some()
    }
}

impl McpServer {
    /// Create a new server, capturing the working directory used for `Current` session resolution.
    #[must_use]
    pub fn new() -> Self {
        Self::with_session_start(false)
    }

    /// Create a server and explicitly configure whether agents may spawn detached sessions.
    #[must_use]
    pub fn with_session_start(allow_session_start: bool) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            cwd,
            allow_session_start,
            session_start: Arc::new(tokio::sync::Semaphore::new(1)),
            session_start_budget: Arc::new(tokio::sync::Semaphore::new(
                MAX_SESSION_STARTS_PER_WINDOW,
            )),
            tool_router: Self::tool_router(),
        }
    }

    async fn mutate(
        &self,
        session: Option<String>,
        request: Request,
        service: &str,
    ) -> ToolResult<MutationResult> {
        let resolved = select::resolve(&self.cwd, session)
            .await
            .map_err(error_data)?;
        let log_cursor = capture_mutation_cursor(&resolved.endpoint, service).await;
        let mut conn = SessionConn::connect(&resolved.endpoint)
            .await
            .map_err(error_data)?;
        let response = conn.request(request).await.map_err(error_data)?;
        let acks = service_result(service, convert::accepted(response)).await?;
        let generation = acks
            .iter()
            .find(|ack| ack.service == service)
            .map(|ack| ack.observed_generation);
        Ok(Json(MutationResult {
            session_ref: SessionRef::from(&resolved.info),
            accepted: acks,
            service: service.to_string(),
            generation,
            log_cursor,
        }))
    }

    async fn start_for_target(&self, target: PathBuf) -> Result<StartSessionResult, ErrorData> {
        if !self.allow_session_start {
            return Err(ErrorData::invalid_params(
                "session startup is disabled; launch `micromux mcp --allow-session-start` to permit process spawning",
                None,
            ));
        }
        let _serial = Arc::clone(&self.session_start)
            .try_acquire_owned()
            .map_err(|_| {
                ErrorData::invalid_params("another session-start request is in progress", None)
            })?;
        let budget = Arc::clone(&self.session_start_budget)
            .try_acquire_owned()
            .map_err(|_| {
                ErrorData::invalid_params(
                    "session-start rate limit reached; wait before spawning another session",
                    None,
                )
            })?;
        tokio::spawn(async move {
            tokio::time::sleep(SESSION_START_WINDOW).await;
            drop(budget);
        });
        self.start_for_target_unchecked(target).await
    }

    async fn start_for_target_unchecked(
        &self,
        target: PathBuf,
    ) -> Result<StartSessionResult, ErrorData> {
        if !transport_supported() {
            return Err(error_data(ToolError::Unsupported));
        }
        let config_path = select::config_for_target(&target).await.ok_or_else(|| {
            ErrorData::invalid_params(
                format!("no micromux config found at or above {}", target.display()),
                None,
            )
        })?;
        let dir_statuses = runtime_dir_statuses();
        let runtime_dirs = usable_runtime_dirs(&dir_statuses);
        if runtime_dirs.is_empty() {
            let diagnostics = select::discovery_diagnostics(
                &self.cwd,
                "no runtime directory could be resolved".to_string(),
                Some(&config_path),
                &dir_statuses,
                Vec::new(),
            );
            return Err(ErrorData::internal_error(
                "no runtime directory could be resolved",
                serde_json::to_value(diagnostics).ok(),
            ));
        }
        let endpoints = runtime_dirs
            .iter()
            .map(|runtime_dir| endpoint_for(runtime_dir, &config_path))
            .collect::<Vec<_>>();

        // If a session answers, this is a no-op. Otherwise let `serve` try the lifetime lock: that
        // keeps duplicate prevention in one place and lets it reclaim stale/bad socket files.
        if let Some(report) = already_running_any(&endpoints, &config_path)
            .await
            .map_err(error_data)?
        {
            return Ok(report);
        }

        let spawned = spawn_detached_serve(&config_path).map_err(|err| {
            // On a platform without the control transport, surface the canonical unsupported error
            // rather than a generic spawn failure.
            if err.kind() == std::io::ErrorKind::Unsupported {
                error_data(ToolError::Unsupported)
            } else {
                ErrorData::internal_error(format!("failed to spawn `micromux serve`: {err}"), None)
            }
        })?;
        let mut child = spawned.child;
        let stderr_path = spawned.stderr_path;
        let mut deadline = tokio::time::Instant::now() + START_READY_TIMEOUT;
        let mut child_exit: Option<std::process::ExitStatus> = None;
        loop {
            // Observe early exit before deciding whether an answering session is ours; after the
            // child is reaped, its pid is no longer a trustworthy ownership signal.
            if child_exit.is_none()
                && let Ok(Some(status)) = child.try_wait()
            {
                child_exit = Some(status);
                deadline = deadline.min(tokio::time::Instant::now() + CHILD_EXIT_GRACE);
            }
            let child_pid = child_exit.is_none().then(|| child.id()).flatten();
            match reachable_session_for_start(&endpoints, &config_path, child_pid).await {
                Ok(Some((started, info))) => {
                    // Reachable. Report `started` only when the answering process is the child this
                    // call spawned; otherwise a concurrent start won the endpoint lock. If our child
                    // is still around in that case, stop it so a duplicate session cannot linger.
                    if !started && child_exit.is_none() {
                        let _ = child.kill().await;
                    }
                    remove_captured_stderr(&stderr_path);
                    return Ok(start_session_report(&info, started));
                }
                Ok(None) => {}
                Err(err) => {
                    // A discovery error (e.g. an ambiguous selector) is caller-actionable and has
                    // nothing to do with the child's stderr; keep the typed error.
                    if child_exit.is_none() {
                        let _ = child.kill().await;
                    }
                    remove_captured_stderr(&stderr_path);
                    return Err(error_data(err));
                }
            }
            // If our child exited, keep polling briefly. A concurrent start_session that won the
            // lifetime-lock race makes our loser child exit too, and the winner may still be
            // binding.
            if tokio::time::Instant::now() >= deadline {
                if child_exit.is_none() {
                    // Still running but never bound — don't leak a session that may hold the ports.
                    let _ = child.kill().await;
                }
                let base = match child_exit {
                    Some(status) => format!(
                        "`micromux serve` for {} exited ({status}) before becoming reachable",
                        config_path.display(),
                    ),
                    None => format!(
                        "`micromux serve` for {} did not become reachable within {}s and was stopped",
                        config_path.display(),
                        START_READY_TIMEOUT.as_secs(),
                    ),
                };
                return Err(ErrorData::internal_error(
                    start_failure_message(&base, &stderr_path),
                    None,
                ));
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    fn require_dynamic_capability(info: &SessionInfo) -> Result<(), ErrorData> {
        let enabled = info
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.dynamic_services.as_ref())
            .is_some();
        if enabled {
            Ok(())
        } else {
            Err(ErrorData::invalid_params(
                format!(
                    "PolicyDenied: dynamic services are disabled for this session; set \
                     control.dynamic_services.enabled: true in {} and restart the session",
                    info.config_path
                ),
                None,
            ))
        }
    }

    /// Shared pipeline for the three dynamic-service tools: resolve, capability pre-flight (a
    /// courtesy — the session enforces regardless), send, convert, wrap with the session ref.
    async fn dynamic_mutation(
        &self,
        session: Option<String>,
        service: &str,
        request: Request,
    ) -> Result<DynamicMutation, ErrorData> {
        let resolved = select::resolve(&self.cwd, session)
            .await
            .map_err(error_data)?;
        Self::require_dynamic_capability(&resolved.info)?;
        let response = send_request(&resolved.endpoint, request)
            .await
            .map_err(error_data)?;
        let receipt = service_result(service, convert::dynamic_service(response)).await?;
        Ok(DynamicMutation { resolved, receipt })
    }
}

/// Captures the optional pre-mutation cursor on a disposable connection.
///
/// A timed-out read can leave a late response in the framing buffer. Keeping this observation
/// separate ensures neither its result nor its transport state can prevent or corrupt the
/// mutation.
async fn capture_mutation_cursor(endpoint: &ControlEndpoint, service: &str) -> Option<u64> {
    let capture = async {
        let mut conn = SessionConn::connect(endpoint).await?;
        current_service_cursor(&mut conn, service).await
    };
    if let Ok(cursor) = tokio::time::timeout(MUTATION_CURSOR_TIMEOUT, capture).await {
        return best_effort_mutation_cursor(service, cursor);
    }
    tracing::warn!(
        service,
        timeout_ms = %MUTATION_CURSOR_TIMEOUT.as_millis(),
        "timed out capturing the pre-mutation log cursor"
    );
    None
}

fn best_effort_mutation_cursor(service: &str, cursor: Result<u64, ToolError>) -> Option<u64> {
    match cursor {
        Ok(cursor) => Some(cursor),
        Err(err) => {
            tracing::warn!(
                service,
                error = %err,
                "could not capture the pre-mutation log cursor"
            );
            None
        }
    }
}

#[tool_router(router = tool_router)]
impl McpServer {
    #[tool(
        description = "List all micromux sessions currently running for this user, with their \
        name, pid, config path, working directory, advertised capabilities, and discovery \
        diagnostics."
    )]
    async fn list_sessions(&self) -> ToolResult<SessionListResult> {
        let list = select::list_sessions().await.map_err(error_data)?;
        Ok(Json(SessionListResult {
            sessions: list.sessions,
            diagnostics: list.diagnostics,
        }))
    }

    #[tool(
        description = "Validate a micromux config without starting a session. Runs the same parse, \
        service normalization, environment-file/interpolation, port, and dependency-graph checks \
        used at startup and returns structured plus ANSI-free rendered diagnostics. Environment \
        interpolation uses the MCP process environment, so unset-variable warnings can differ \
        from the eventual session environment."
    )]
    async fn validate_config(
        &self,
        args: Parameters<ValidateConfigArgs>,
    ) -> ToolResult<micromux::ValidationReport> {
        let Parameters(args) = args;
        let target = args
            .path
            .as_deref()
            .map_or_else(|| self.cwd.clone(), PathBuf::from);
        let config_path = select::config_for_target(&target).await.ok_or_else(|| {
            ErrorData::invalid_params(
                format!("no micromux config found at or above {}", target.display()),
                None,
            )
        })?;
        let report = tokio::task::spawn_blocking(move || {
            // `None` lets the config's own `strict:` key decide, matching session startup.
            micromux::validate_config_file(&config_path, None)
        })
        .await
        .map_err(|err| ErrorData::internal_error(format!("validation task failed: {err}"), None))?
        .map_err(|err| ErrorData::invalid_params(err.to_string(), None))?;
        Ok(Json(report))
    }

    #[tool(
        description = "Reconcile a running session's configured services with its on-disk \
        micromux.yaml. Run with dry_run=true first to inspect the semantic add/remove/change diff; \
        validate_config provides a no-session pre-flight. Changed services are not restarted, so \
        follow with restart_service or restart_service_and_wait when the new definition should run."
    )]
    async fn reconcile_config(
        &self,
        args: Parameters<ReconcileConfigArgs>,
    ) -> ToolResult<ReconcileConfigResult> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let response = send_request(
            &resolved.endpoint,
            Request::ReconcileConfig {
                dry_run: args.dry_run,
            },
        )
        .await
        .map_err(error_data)?;
        let receipt = convert::reconcile(response).map_err(error_data)?;
        Ok(Json(ReconcileConfigResult {
            session_ref: SessionRef::from(&resolved.info),
            dry_run: receipt.dry_run,
            actions: receipt.actions,
        }))
    }

    #[tool(
        description = "List the services in a session with their desired/execution state, health, \
        advertised ports, pid, start time, config drift, uptime, restart state/policy, last exit \
        code, run generation, configured/dynamic origin and revision, resolved command (argv), and \
        working directory. The result carries a copy-pasteable session_selector."
    )]
    async fn list_services(&self, args: Parameters<SessionArgs>) -> ToolResult<ServiceListResult> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let response = send_request(&resolved.endpoint, Request::ListServices)
            .await
            .map_err(error_data)?;
        let services = convert::services(response).map_err(error_data)?;
        let session_selector = session_selector(&resolved.info);
        Ok(Json(ServiceListResult {
            config_path: resolved.info.config_path,
            session: resolved.info.name,
            session_selector,
            services,
        }))
    }

    #[tool(
        description = "Locate a service by id or name across every running micromux session. \
        Returns each matching session's copy-pasteable selector, config path, working directory, \
        and the service's current snapshot (state, health, run generation, command, advertised \
        ports and configured/dynamic origin), so dynamic services appear like every other roster \
        entry and can be targeted without the list_sessions -> pick a hash -> list_services dance."
    )]
    async fn find_service(
        &self,
        args: Parameters<FindServiceArgs>,
    ) -> ToolResult<FindServiceResult> {
        let Parameters(args) = args;
        // Source sessions directly so a hard discovery error (e.g. unsupported platform) is
        // surfaced rather than masked as "no matches"; match by id or name for discovery.
        let sessions = select::answering_sessions().await.map_err(error_data)?;
        let mut matches = Vec::new();
        for resolved in sessions {
            let Some(snapshot) = service_in_session(&resolved, &args.service).await else {
                continue;
            };
            let session_selector = session_selector(&resolved.info);
            let info = resolved.info;
            matches.push(ServiceLocation {
                session: info.name,
                session_selector,
                pid: info.pid,
                config_path: info.config_path,
                working_dir: info.working_dir,
                snapshot: Some(snapshot),
            });
        }
        Ok(Json(FindServiceResult {
            service: args.service,
            matches,
        }))
    }

    #[tool(
        description = "Read recent log entries for one service, or pass service=\"*\" to merge the \
        current visible logs from all services with timestamp-guided, cursor-safe ordering. Omit \
        run_generation for the visible bounded log stream; pass a retained run_generation from list_log_runs to inspect a \
        bounded tail of a current or previous disk-backed run (single-service only). Use follow_logs to page through a \
        retained run with a cursor. ANSI color is stripped by default (raw=true keeps it), terminal \
        padding is trimmed, and tail counts logical log entries, not wrapped terminal rows. Filter \
        with grep (regex), grep_context, since, trace_id, or, for JSON-log services, min_level. Use \
        format=\"compact\" for token-efficient structured JSON logs; entries carry micromux \
        ingestion timestamps, a detected `level`, optional parsed service timestamp, and parsed \
        `message`/`fields` for structured JSON logs."
    )]
    async fn get_logs(&self, args: Parameters<LogsArgs>) -> ToolResult<LogsResult> {
        let Parameters(args) = args;
        let filters = LogFilters::from_args(&args.filters)?;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let filtering = filters.filters_records();
        // A retained disk run defaults to a wide window (matching the session's own default); the
        // bounded visible stream defaults to 200.
        let default_tail = if args.run_generation.is_some() {
            MAX_LOG_TAIL
        } else {
            DEFAULT_LOG_TAIL
        };
        let requested_tail = args.tail.unwrap_or(default_tail).min(MAX_LOG_TAIL);
        // When filtering, scan the whole window so matches aren't limited to the last `tail` lines.
        let fetch_tail = if filtering {
            MAX_LOG_TAIL
        } else {
            requested_tail
        };
        if args.service == "*" {
            if args.run_generation.is_some() {
                return Err(ErrorData::invalid_params(
                    "service=\"*\" only supports the current visible log stream; retained \
                     run_generation is service-scoped",
                    None,
                ));
            }
            let mut conn = SessionConn::connect(&resolved.endpoint)
                .await
                .map_err(error_data)?;
            let response = conn
                .request(Request::ListServices)
                .await
                .map_err(error_data)?;
            let services = convert::services(response).map_err(error_data)?;
            let mut entries = Vec::new();
            let mut truncated = false;
            for service in services {
                let (mut shaped, window_full, server_truncated) = fetch_and_shape_tail(
                    &mut conn,
                    &service.id,
                    None,
                    fetch_tail,
                    &filters,
                    args.filters.raw,
                )
                .await?;
                truncated |= server_truncated || (window_full && filtering);
                for entry in &mut shaped {
                    entry.service = Some(service.id.clone());
                }
                entries.extend(shaped);
            }
            let mut entries = logproc::merge_preserving_service_order(entries);
            if entries.len() > requested_tail {
                truncated |=
                    logproc::tail_preserving_record_boundaries(&mut entries, requested_tail);
            }
            return Ok(Json(LogsResult {
                service: args.service,
                run_generation: args.run_generation,
                config_path: resolved.info.config_path,
                entries,
                truncated,
            }));
        }
        let mut conn = SessionConn::connect(&resolved.endpoint)
            .await
            .map_err(error_data)?;
        let (mut entries, window_full, server_truncated) = fetch_and_shape_tail(
            &mut conn,
            &args.service,
            args.run_generation,
            fetch_tail,
            &filters,
            args.filters.raw,
        )
        .await?;
        // A full fetched window that was filtered, or that yielded fewer entries than asked, may
        // hide older matches/entries beyond the scan — don't report a capped scan as complete.
        let mut truncated =
            server_truncated || (window_full && (filtering || entries.len() < requested_tail));
        // Tail here rather than inside shape so that a record splitting into more entries than
        // requested_tail is reported as truncated instead of silently dropping older records.
        if entries.len() > requested_tail {
            truncated |= logproc::tail_preserving_record_boundaries(&mut entries, requested_tail);
        }
        Ok(Json(LogsResult {
            service: args.service,
            run_generation: args.run_generation,
            config_path: resolved.info.config_path,
            entries,
            truncated,
        }))
    }

    #[tool(
        description = "List retained log runs for a service, including run generations and \
        sequence ranges. Use a returned run_generation with get_logs/follow_logs to inspect or \
        page through disk-backed run logs."
    )]
    async fn list_log_runs(&self, args: Parameters<ServiceArgs>) -> ToolResult<LogRunsResult> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let response = send_request(
            &resolved.endpoint,
            Request::ListLogRuns {
                service: args.service.clone(),
            },
        )
        .await
        .map_err(error_data)?;
        let runs = service_result(&args.service, convert::log_runs(response)).await?;
        Ok(Json(LogRunsResult {
            service: args.service,
            config_path: resolved.info.config_path,
            runs,
        }))
    }

    #[tool(
        description = "Return the disk JSONL file path for a retained service run. Omit \
        run_generation to return the current retained run. This is intended as a fallback for \
        shell-based inspection when MCP log tools are unavailable."
    )]
    async fn get_log_file_path(
        &self,
        args: Parameters<LogFilePathArgs>,
    ) -> ToolResult<LogFilePathResult> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let response = send_request(
            &resolved.endpoint,
            Request::ListLogRuns {
                service: args.service.clone(),
            },
        )
        .await
        .map_err(error_data)?;
        let runs = service_result(&args.service, convert::log_runs(response)).await?;
        let run = match args.run_generation {
            Some(run_generation) => runs
                .into_iter()
                .find(|run| run.run_generation == run_generation),
            None => runs.into_iter().find(|run| run.current),
        }
        .ok_or_else(|| {
            error_data(ToolError::Remote {
                code: ErrorCode::UnknownRun,
                message: format!(
                    "service `{}` has no retained run `{}`",
                    args.service,
                    args.run_generation.map_or_else(
                        || "current".to_string(),
                        |generation| generation.to_string()
                    )
                ),
            })
        })?;
        let path = run.path.clone().ok_or_else(|| {
            error_data(ToolError::InvalidState(format!(
                "service `{}` run `{}` is not disk-backed",
                args.service, run.run_generation
            )))
        })?;
        Ok(Json(LogFilePathResult {
            service: args.service,
            config_path: resolved.info.config_path,
            run_generation: run.run_generation,
            path,
        }))
    }

    #[tool(
        description = "Return the current visible-log cursor for every service. Call this before \
        an action, then pass the returned `cursors` as `after` to follow_all_logs to see what the \
        action caused across services. Services with no entries return cursor 0."
    )]
    async fn log_cursors(&self, args: Parameters<SessionArgs>) -> ToolResult<LogCursorsResult> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let mut conn = SessionConn::connect(&resolved.endpoint)
            .await
            .map_err(error_data)?;
        let cursors = current_cursors(&mut conn).await.map_err(error_data)?;
        Ok(Json(LogCursorsResult {
            config_path: resolved.info.config_path,
            cursors,
        }))
    }

    #[tool(
        description = "Restart a service. Returns the run generation *before* the restart; pass \
        it to wait_for_healthy as after_generation. When available, log_cursor is the pre-restart \
        log position; pass it to follow_logs as after_seq to read exactly the new run's output. \
        Cursor capture is best-effort and never prevents the restart. Reloads the latest micromux \
        config before spawning the replacement, so edited command flags, healthchecks, and log \
        retention take effect. Restarting a disabled service is rejected."
    )]
    async fn restart_service(&self, args: Parameters<ServiceArgs>) -> ToolResult<MutationResult> {
        let Parameters(args) = args;
        self.mutate(
            args.session,
            Request::Restart {
                service: args.service.clone(),
            },
            &args.service,
        )
        .await
    }

    #[tool(
        description = "Convenience wrapper that captures log cursors, restarts one service, waits \
        for the replacement run, and returns its snapshot plus compact new logs, lifecycle events, \
        and failure diagnosis. The primitive tools remain the right choice for finer control. Do not use this \
        for hot-reload services, which never restart; use log_cursors followed by follow_all_logs \
        for those."
    )]
    async fn restart_service_and_wait(
        &self,
        args: Parameters<RestartAndWaitArgs>,
    ) -> ToolResult<RestartAndWaitResult> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let timeout = Duration::from_secs(
            args.timeout_secs
                .unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS)
                .min(MAX_WAIT_TIMEOUT_SECS),
        );
        let log_limit = args
            .log_limit
            .unwrap_or(DEFAULT_RESTART_LOG_LIMIT)
            .clamp(1, MAX_RESTART_LOG_LIMIT);
        let mut conn = SessionConn::connect(&resolved.endpoint)
            .await
            .map_err(error_data)?;

        let before = service_result(
            &args.service,
            service_snapshot(&mut conn, &args.service).await,
        )
        .await?;
        // Reject before capturing cursors, mirroring restart_service's remote rejection of
        // disabled services (which still backstops the race where the service is disabled between
        // this check and the restart request).
        if before.desired == Desired::Disabled {
            return Err(error_data(ToolError::InvalidState(format!(
                "service `{}` is disabled and cannot be restarted; use enable_service to start it",
                args.service
            ))));
        }
        let cursors = current_cursors(&mut conn).await.map_err(error_data)?;
        let response = conn
            .request(Request::Restart {
                service: args.service.clone(),
            })
            .await
            .map_err(error_data)?;
        let acks = service_result(&args.service, convert::accepted(response)).await?;
        let previous_generation = acks
            .iter()
            .find(|ack| ack.service == args.service)
            .map(|ack| ack.observed_generation)
            .ok_or_else(|| {
                error_data(ToolError::Unexpected(format!(
                    "restart acknowledgement omitted service `{}`",
                    args.service
                )))
            })?;

        let conclusion = service_result(
            &args.service,
            wait_for_health(
                &resolved.endpoint,
                &mut conn,
                &args.service,
                Some(previous_generation),
                timeout,
            )
            .await,
        )
        .await?;
        let snapshot = conclusion.snapshot().clone();
        let cursor = cursors.get(&args.service).copied().unwrap_or(0);
        let (logs, logs_truncated) = service_result(
            &args.service,
            compact_logs_since(&mut conn, &args.service, cursor, log_limit).await,
        )
        .await?;
        let diagnosis = if conclusion.needs_diagnosis() {
            Some(
                service_result(
                    &args.service,
                    diagnose_service(&mut conn, snapshot.clone(), DEFAULT_DIAGNOSE_TAIL).await,
                )
                .await?,
            )
        } else {
            None
        };
        let events = service_events_tail(&mut conn, &args.service)
            .await?
            .into_iter()
            .filter(|event| event.run_generation >= previous_generation)
            .collect();
        let outcome = WaitResult::from_conclusion(args.service.clone(), timeout, &conclusion);

        Ok(Json(RestartAndWaitResult {
            session_ref: SessionRef::from(&resolved.info),
            service: args.service,
            previous_generation,
            run_generation: snapshot.run_generation,
            outcome,
            snapshot,
            logs,
            logs_truncated,
            events,
            diagnosis,
        }))
    }

    #[tool(
        description = "Restart all enabled services in a session, including live dynamic services \
        (disabled and retired services are skipped). Reloads the latest micromux config before \
        requesting restarts. This receipt has no per-service log cursor; use log_cursors before \
        restart_all when you need logs since the action."
    )]
    async fn restart_all(
        &self,
        args: Parameters<SessionArgs>,
    ) -> ToolResult<SessionMutationResult> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let response = send_request(&resolved.endpoint, Request::RestartAll)
            .await
            .map_err(error_data)?;
        let acks = convert::accepted(response).map_err(error_data)?;
        Ok(Json(SessionMutationResult {
            session_ref: SessionRef::from(&resolved.info),
            accepted: acks,
        }))
    }

    #[tool(
        description = "Enable (and start) a service. Returns the run generation before enabling \
        plus the pre-enable log position in log_cursor when cursor capture succeeds (pass it to \
        follow_logs as after_seq). Cursor capture is best-effort and never prevents the enable. \
        Reloads the latest micromux config before spawning, so edited command flags, healthchecks, \
        and log retention take effect."
    )]
    async fn enable_service(&self, args: Parameters<ServiceArgs>) -> ToolResult<MutationResult> {
        let Parameters(args) = args;
        self.mutate(
            args.session,
            Request::Enable {
                service: args.service.clone(),
            },
            &args.service,
        )
        .await
    }

    #[tool(description = "Disable a service (stop it and keep it stopped).")]
    async fn disable_service(&self, args: Parameters<ServiceArgs>) -> ToolResult<MutationResult> {
        let Parameters(args) = args;
        self.mutate(
            args.session,
            Request::Disable {
                service: args.service.clone(),
            },
            &args.service,
        )
        .await
    }

    #[tool(
        description = "Create and start a runtime service in the target session. Requires \
        control.dynamic_services.enabled in that session's micromux.yaml. Dynamic services stop \
        with the session; leases are bounded unless policy explicitly permits expires_after=\"none\". \
        from_service clones any existing service (configured or dynamic) server-side, including \
        environment values that are never displayed. When replacing, pass from_service equal to \
        service to start from its current spec and override only provided fields. Put stable flags \
        in command: extra_args re-append on every self-referential replace (and for CMD-SHELL they \
        are shell positional parameters, not appended script text). Pass idempotency_key when \
        retrying. Then call wait_for_healthy with after_generation=observed_generation."
    )]
    async fn start_dynamic_service(
        &self,
        args: Parameters<DynamicServiceArgs>,
    ) -> ToolResult<DynamicMutationResult> {
        let Parameters(args) = args;
        let service = args.params.service.clone();
        let mutation = self
            .dynamic_mutation(
                args.session,
                &service,
                Request::StartDynamicService {
                    params: args.params,
                },
            )
            .await?;
        Ok(Json(mutation.into_result()))
    }

    #[tool(
        description = "Create a runtime service, wait for its existing or newly created run, and \
        return the mutation receipt, compact logs, lifecycle events, and failure diagnosis. This \
        preserves start_dynamic_service policy and idempotency semantics; an idempotent replay \
        waits on the original run. Use the primitive tools when finer control is needed."
    )]
    async fn start_dynamic_service_and_wait(
        &self,
        args: Parameters<StartDynamicAndWaitArgs>,
    ) -> ToolResult<StartDynamicAndWaitResult> {
        let Parameters(args) = args;
        let service = args.params.service.clone();
        let timeout = Duration::from_secs(
            args.timeout_secs
                .unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS)
                .min(MAX_WAIT_TIMEOUT_SECS),
        );
        let log_limit = args
            .log_limit
            .unwrap_or(DEFAULT_RESTART_LOG_LIMIT)
            .clamp(1, MAX_RESTART_LOG_LIMIT);
        let mutation = self
            .dynamic_mutation(
                args.session,
                &service,
                Request::StartDynamicService {
                    params: args.params,
                },
            )
            .await?;
        let mut conn = SessionConn::connect(&mutation.resolved.endpoint)
            .await
            .map_err(error_data)?;
        let conclusion = service_result(
            &service,
            wait_for_health(
                &mutation.resolved.endpoint,
                &mut conn,
                &service,
                Some(mutation.receipt.observed_generation),
                timeout,
            )
            .await,
        )
        .await?;
        let snapshot = conclusion.snapshot().clone();
        let (logs, logs_truncated) = service_result(
            &service,
            compact_logs_since(&mut conn, &service, 0, log_limit).await,
        )
        .await?;
        let diagnosis = if conclusion.needs_diagnosis() {
            Some(
                service_result(
                    &service,
                    diagnose_service(&mut conn, snapshot.clone(), DEFAULT_DIAGNOSE_TAIL).await,
                )
                .await?,
            )
        } else {
            None
        };
        // Events only accompany a failed wait, mirroring the diagnosis block: on a healthy path
        // — including an idempotent replay of a long-lived service — the full retained history
        // would be noise, not evidence.
        let events = if conclusion.needs_diagnosis() {
            service_events_tail(&mut conn, &service).await?
        } else {
            Vec::new()
        };
        let outcome = WaitResult::from_conclusion(service, timeout, &conclusion);

        Ok(Json(StartDynamicAndWaitResult {
            session_ref: SessionRef::from(&mutation.resolved.info),
            receipt: mutation.receipt,
            outcome,
            snapshot,
            logs,
            logs_truncated,
            events,
            diagnosis,
        }))
    }

    #[tool(
        description = "Replace or revive a runtime service using optimistic concurrency. Requires \
        control.dynamic_services.enabled in the target session's micromux.yaml. Dynamic services \
        stop with the session and receive a fresh bounded or explicitly unbounded lease after \
        replacement. from_service clones any existing service server-side, including environment \
        values that are never displayed. Pass from_service equal to service to start from its \
        current spec and override only provided fields. Put stable flags in command: extra_args \
        re-append on every self-referential replace. Pass idempotency_key when retrying, then \
        compose with wait_for_healthy(after_generation=observed_generation)."
    )]
    async fn replace_dynamic_service(
        &self,
        args: Parameters<ReplaceDynamicServiceArgs>,
    ) -> ToolResult<DynamicMutationResult> {
        let Parameters(args) = args;
        let service = args.params.service.clone();
        let request = Request::ReplaceDynamicService {
            service: service.clone(),
            expected_revision: args.expected_revision,
            params: args.params,
        };
        let mutation = self
            .dynamic_mutation(args.session, &service, request)
            .await?;
        Ok(Json(mutation.into_result()))
    }

    #[tool(
        description = "Renew a live runtime service's lease without restarting it or changing its \
        revision. Requires control.dynamic_services.enabled in the target session's \
        micromux.yaml. Omit expires_after to apply the policy default; request 'none' for a \
        session-lifetime lease where policy permits it. A receipt replayed from an earlier \
        idempotent create may show a stale expiry after renewal; the service snapshot is the \
        expiry authority."
    )]
    async fn renew_dynamic_service(
        &self,
        args: Parameters<RenewDynamicServiceArgs>,
    ) -> ToolResult<DynamicMutationResult> {
        let Parameters(args) = args;
        let request = Request::RenewDynamicService {
            service: args.service.clone(),
            expected_revision: args.expected_revision,
            expires_after: args.expires_after,
        };
        let mutation = self
            .dynamic_mutation(args.session, &args.service, request)
            .await?;
        Ok(Json(mutation.into_result()))
    }

    #[tool(
        description = "Retire a runtime service while preserving its bounded logs and snapshot for \
        post-mortem inspection. Requires control.dynamic_services.enabled in the target session's \
        micromux.yaml. Stop is idempotent; the service also stops with the session, and its lease \
        (unless the policy permitted expires_after=\"none\") would retire it on expiry anyway."
    )]
    async fn stop_dynamic_service(
        &self,
        args: Parameters<ServiceArgs>,
    ) -> ToolResult<DynamicMutationResult> {
        let Parameters(args) = args;
        let request = Request::StopDynamicService {
            service: args.service.clone(),
        };
        let mutation = self
            .dynamic_mutation(args.session, &args.service, request)
            .await?;
        Ok(Json(mutation.into_result()))
    }

    #[tool(
        description = "Stop a whole micromux session: every service is stopped and the session \
        process exits (graceful, like Ctrl-C), freeing its ports. Select with `session` (see \
        list_sessions); omit for the current project. Use this before start_session for another \
        worktree that binds the same ports. Returns `stopped: true` once the process has exited."
    )]
    async fn stop_session(&self, args: Parameters<SessionArgs>) -> ToolResult<StopSessionResult> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        #[cfg(unix)]
        let process_exit = ProcessExitWatch::new(resolved.info.pid);
        let response = send_request(&resolved.endpoint, Request::Shutdown)
            .await
            .map_err(error_data)?;
        convert::shutting_down(response).map_err(error_data)?;
        // Confirm the session process has exited (its supervised services were stopped) before
        // returning, so the caller can usually start another session on the same ports. A service
        // that detaches from the session's process group can still outlive it — this is not a hard
        // guarantee every port is freed.
        #[cfg(unix)]
        let stopped = process_exit.wait(STOP_CONFIRM_TIMEOUT).await;
        #[cfg(not(unix))]
        let stopped = true;
        let note = if stopped {
            None
        } else {
            Some(
                "shutdown was acknowledged but the process was still terminating after the confirm \
                 window; its ports may take another moment to free",
            )
        };
        Ok(Json(StopSessionResult {
            stopped,
            session_ref: SessionRef::from(&resolved.info),
            note,
        }))
    }

    #[tool(
        description = "Start a new headless micromux session for a project (brings its services \
        up). Spawns `micromux serve` detached for the project's config and returns once the session \
        is reachable; a no-op if one is already running for that config. `path` is a project \
        directory or a config file — omit for the MCP server's directory. If another worktree's \
        session binds the same ports, stop it first with stop_session."
    )]
    async fn start_session(&self, args: Parameters<StartArgs>) -> ToolResult<StartSessionResult> {
        let Parameters(args) = args;
        let target = args
            .path
            .as_deref()
            .map_or_else(|| self.cwd.clone(), PathBuf::from);
        self.start_for_target(target).await.map(Json)
    }

    #[tool(
        description = "Read log entries after a cursor for incremental following. Returns the new \
        entries and a next_seq; pass next_seq as after_seq on the next call. If retention already \
        evicted unread entries, the response includes a gap object. Pass run_generation for a full \
        disk-backed retained run page; omit it for the bounded visible stream. For the visible \
        stream, omitting after_seq returns the latest tail; pass 0 to start at the first retained \
        visible entry. Supports the same raw/grep/grep_context/min_level/since/trace_id/format filters as get_logs; next_seq still \
        advances past filtered-out entries."
    )]
    async fn follow_logs(&self, args: Parameters<FollowArgs>) -> ToolResult<FollowLogsResult> {
        let Parameters(args) = args;
        let filters = LogFilters::from_args(&args.filters)?;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let response = send_request(
            &resolved.endpoint,
            Request::FollowLogs {
                service: args.service.clone(),
                run_generation: args.run_generation,
                after: args.after_seq,
            },
        )
        .await
        .map_err(error_data)?;
        let logs = service_result(&args.service, convert::logs(response)).await?;
        // Cursor + gap are computed from the raw records (seq is per record), so `next_seq` advances
        // past filtered-out entries and following never re-fetches them.
        let next_seq = next_follow_cursor(&logs.lines, args.after_seq);
        let gap = follow_gap(
            &logs.lines,
            logs.first_retained_seq,
            args.after_seq,
            args.run_generation,
        );
        let entries = logproc::shape(&logs.lines, &filters.shape(args.filters.raw, None));
        Ok(Json(FollowLogsResult {
            service: args.service,
            run_generation: args.run_generation,
            entries,
            next_seq,
            gap,
            truncated: logs.truncated,
        }))
    }

    #[tool(
        description = "Follow current visible logs across all services with a per-service cursor \
        map. Pass `after` from log_cursors or from the previous follow_all_logs `next` value. \
        Returns entries merged by parsed service timestamp when present, otherwise micromux \
        ingestion timestamp, while preserving each service's cursor order. This is additive to \
        follow_logs; retained run paging stays single-service because run_generation and seq are \
        service-scoped. Supports the same raw/grep/grep_context/min_level/since/trace_id/format filters as \
        get_logs."
    )]
    async fn follow_all_logs(
        &self,
        args: Parameters<FollowAllArgs>,
    ) -> ToolResult<FollowAllLogsResult> {
        let Parameters(args) = args;
        let filters = LogFilters::from_args(&args.filters)?;
        let raw = args.filters.raw;
        let limit = args
            .limit
            .unwrap_or(DEFAULT_LOG_TAIL)
            .clamp(1, MAX_LOG_TAIL);
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let mut conn = SessionConn::connect(&resolved.endpoint)
            .await
            .map_err(error_data)?;
        let merged = follow_all_current_logs(&mut conn, &args.after, raw, &filters, limit)
            .await
            .map_err(error_data)?;
        Ok(Json(FollowAllLogsResult {
            config_path: resolved.info.config_path,
            entries: merged.entries,
            next: merged.next,
            gaps: merged.gaps,
            truncated: merged.truncated,
        }))
    }

    #[tool(
        description = "Wait until a matching log entry appears, then return the matching entries \
        and updated cursors. By default this starts at the current end of the log stream, making it \
        useful for 'perform an action, then wait for the backend log evidence'. Pass service=\"*\" \
        with `after` from log_cursors/follow_all_logs to wait across all services. Supports the \
        same raw/grep/grep_context/min_level/since/trace_id/format filters as get_logs."
    )]
    async fn wait_for_log(&self, args: Parameters<WaitLogArgs>) -> ToolResult<WaitForLogResult> {
        let Parameters(args) = args;
        let filters = LogFilters::from_args(&args.filters)?;
        let raw = args.filters.raw;
        let limit = args.limit.unwrap_or(20).clamp(1, MAX_LOG_TAIL);
        let timeout = Duration::from_secs(
            args.timeout_secs
                .unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS)
                .min(MAX_WAIT_TIMEOUT_SECS),
        );
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let mut conn = SessionConn::connect(&resolved.endpoint)
            .await
            .map_err(error_data)?;
        let deadline = tokio::time::Instant::now() + timeout;

        if args.service == "*" {
            let mut cursors = if args.after.is_empty() {
                current_cursors(&mut conn).await.map_err(error_data)?
            } else {
                args.after
            };
            loop {
                let merged = follow_all_current_logs(&mut conn, &cursors, raw, &filters, limit)
                    .await
                    .map_err(error_data)?;
                if !merged.entries.is_empty() {
                    return Ok(Json(WaitForLogResult {
                        status: WaitForLogStatus::Matched,
                        service: "*".to_string(),
                        config_path: resolved.info.config_path,
                        entries: merged.entries,
                        next_seq: None,
                        next: merged.next,
                        truncated: merged.truncated,
                        waited_secs: None,
                        hint: None,
                    }));
                }
                cursors = merged.next;
                if tokio::time::Instant::now() >= deadline {
                    return Ok(Json(WaitForLogResult {
                        status: WaitForLogStatus::Timeout,
                        service: "*".to_string(),
                        config_path: resolved.info.config_path,
                        entries: Vec::new(),
                        next_seq: None,
                        next: cursors,
                        truncated: merged.truncated,
                        waited_secs: Some(timeout.as_secs()),
                        hint: Some("no matching log entry arrived before timeout_secs"),
                    }));
                }
                tokio::time::sleep(WAIT_LOG_POLL).await;
            }
        }

        let mut cursor = match args.after_seq {
            Some(cursor) => cursor,
            None => current_service_cursor(&mut conn, &args.service)
                .await
                .map_err(error_data)?,
        };
        loop {
            let response = conn
                .request(Request::FollowLogs {
                    service: args.service.clone(),
                    run_generation: None,
                    after: Some(cursor),
                })
                .await
                .map_err(error_data)?;
            let logs = service_result(&args.service, convert::logs(response)).await?;
            let raw_next_seq = next_follow_cursor(&logs.lines, Some(cursor)).unwrap_or(cursor);
            let mut entries = logproc::shape(&logs.lines, &filters.shape(raw, None));
            if !entries.is_empty() {
                let (next_seq, truncated) =
                    truncate_wait_matches(&mut entries, raw_next_seq, logs.truncated, limit);
                return Ok(Json(WaitForLogResult {
                    status: WaitForLogStatus::Matched,
                    service: args.service,
                    config_path: resolved.info.config_path,
                    entries,
                    next_seq: Some(next_seq),
                    next: BTreeMap::new(),
                    truncated,
                    waited_secs: None,
                    hint: None,
                }));
            }
            cursor = raw_next_seq;
            if tokio::time::Instant::now() >= deadline {
                return Ok(Json(WaitForLogResult {
                    status: WaitForLogStatus::Timeout,
                    service: args.service,
                    config_path: resolved.info.config_path,
                    entries: Vec::new(),
                    next_seq: Some(cursor),
                    next: BTreeMap::new(),
                    truncated: logs.truncated,
                    waited_secs: Some(timeout.as_secs()),
                    hint: Some("no matching log entry arrived before timeout_secs"),
                }));
            }
            tokio::time::sleep(WAIT_LOG_POLL).await;
        }
    }

    #[tool(
        description = "Show a service's runtime snapshot and latest healthcheck attempt for its \
        current live run (command, result, and bounded output), plus the session config path."
    )]
    async fn get_health(&self, args: Parameters<ServiceArgs>) -> ToolResult<HealthResult> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let mut conn = SessionConn::connect(&resolved.endpoint)
            .await
            .map_err(error_data)?;
        let snapshot = service_result(
            &args.service,
            service_snapshot(&mut conn, &args.service).await,
        )
        .await?;
        let latest_healthcheck = latest_health_for_snapshot(&mut conn, &snapshot)
            .await
            .map(bounded_attempt);
        Ok(Json(HealthResult {
            config_path: resolved.info.config_path,
            service: args.service,
            snapshot,
            latest_healthcheck,
        }))
    }

    #[tool(
        description = "Show a service's retained healthcheck attempt history for its current or \
        latest run, oldest first, with per-attempt bounded output. Use when the latest attempt \
        from get_health is not enough — e.g. to see whether a flapping probe fails the same way \
        every time."
    )]
    async fn get_health_history(
        &self,
        args: Parameters<ServiceArgs>,
    ) -> ToolResult<HealthHistoryResult> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let mut conn = SessionConn::connect(&resolved.endpoint)
            .await
            .map_err(error_data)?;
        // The snapshot fetch first: it enriches an unknown-service error with sibling-session
        // pointers and gives the caller run context for the attempts.
        let snapshot = service_result(
            &args.service,
            service_snapshot(&mut conn, &args.service).await,
        )
        .await?;
        let attempts = service_result(
            &args.service,
            health_history(&mut conn, &args.service).await,
        )
        .await?
        .into_iter()
        .map(bounded_attempt)
        .collect();
        Ok(Json(HealthHistoryResult {
            config_path: resolved.info.config_path,
            service: args.service,
            snapshot,
            attempts,
        }))
    }

    #[tool(
        description = "Return retained scheduler lifecycle events. Use after_seq for one service, \
        or pass service=\"*\" with the previous per-service `next` map as `after` to merge every \
        service's timeline. Events include dependency gating, spawn/exit, health transitions, \
        restart backoff, config reloads, and dynamic-service lifecycle changes."
    )]
    async fn get_service_events(
        &self,
        args: Parameters<ServiceEventsArgs>,
    ) -> ToolResult<ServiceEventsResult> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        if args.service == "*" {
            if args.after_seq.is_some() {
                return Err(ErrorData::invalid_params(
                    "after_seq is service-scoped; use the per-service after map with service=\"*\"",
                    None,
                ));
            }
            let limit = args
                .tail
                .unwrap_or(DEFAULT_EVENT_TAIL)
                .clamp(1, MAX_EVENT_TAIL);
            let tail_mode = args.after.is_none();
            let mut conn = SessionConn::connect(&resolved.endpoint)
                .await
                .map_err(error_data)?;
            let response = conn
                .request(Request::ListServices)
                .await
                .map_err(error_data)?;
            let services = convert::services(response).map_err(error_data)?;
            let mut pages = Vec::with_capacity(services.len());
            for service in services {
                let after_seq = args
                    .after
                    .as_ref()
                    .map(|cursors| cursors.get(&service.id).copied().unwrap_or(0));
                let response = conn
                    .request(Request::GetEvents {
                        service: service.id.clone(),
                        after: after_seq,
                        tail: Some(limit),
                    })
                    .await
                    .map_err(error_data)?;
                // A service evicted between ListServices and its GetEvents must not fail the
                // whole session-wide merge; skip it and let the survivors report.
                let (events, truncated) = match convert::events(response) {
                    Ok(page) => page,
                    Err(ToolError::Remote {
                        code: ErrorCode::UnknownService,
                        ..
                    }) => continue,
                    Err(err) => return Err(error_data(err)),
                };
                pages.push(ServiceEventPage {
                    service: service.id,
                    after_seq,
                    events,
                    truncated,
                });
            }
            let merged = merge_service_events(pages, limit, tail_mode);
            return Ok(Json(ServiceEventsResult::Session(
                SessionServiceEventsResult {
                    session_ref: SessionRef::from(&resolved.info),
                    service: args.service,
                    events: merged.events,
                    next: merged.next,
                    truncated: merged.truncated,
                },
            )));
        }
        if args.after.is_some() {
            return Err(ErrorData::invalid_params(
                "after is only valid with service=\"*\"; use after_seq for one service",
                None,
            ));
        }
        let response = send_request(
            &resolved.endpoint,
            Request::GetEvents {
                service: args.service.clone(),
                after: args.after_seq,
                tail: args.tail,
            },
        )
        .await
        .map_err(error_data)?;
        let (events, truncated) = service_result(&args.service, convert::events(response)).await?;
        let next_seq = events.last().map(|event| event.seq).or(args.after_seq);
        Ok(Json(ServiceEventsResult::Single(
            SingleServiceEventsResult {
                session_ref: SessionRef::from(&resolved.info),
                service: args.service,
                events,
                next_seq,
                truncated,
            },
        )))
    }

    #[tool(
        description = "Summarize services that need attention in one call. Returns exited, \
        starting/pending/stopping, or unhealthy services with their full state snapshot, current \
        live-run healthcheck output when applicable, recent lifecycle events, factual \
        supervisor-state signals, and a compact tail of likely-cause log lines."
    )]
    async fn diagnose(&self, args: Parameters<DiagnoseArgs>) -> ToolResult<DiagnoseResult> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let tail = args
            .tail_per_service
            .unwrap_or(DEFAULT_DIAGNOSE_TAIL)
            .clamp(1, MAX_DIAGNOSE_TAIL);
        let mut conn = SessionConn::connect(&resolved.endpoint)
            .await
            .map_err(error_data)?;
        let response = conn
            .request(Request::ListServices)
            .await
            .map_err(error_data)?;
        let services = convert::services(response).map_err(error_data)?;
        let mut diagnoses = Vec::new();
        for snapshot in services.into_iter().filter(service_needs_diagnosis) {
            diagnoses.push(
                diagnose_service(&mut conn, snapshot, tail)
                    .await
                    .map_err(error_data)?,
            );
        }
        let session_selector = session_selector(&resolved.info);
        let disk_log_reads = resolved
            .info
            .capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.disk_log_reads);
        Ok(Json(DiagnoseResult {
            config_path: resolved.info.config_path,
            session: resolved.info.name,
            session_selector,
            disk_log_reads,
            services: diagnoses,
        }))
    }

    #[tool(
        description = "Wait until a service becomes healthy (or its run exits, or a timeout). \
        Pass after_generation (from restart_service/enable_service) to wait for the new run."
    )]
    async fn wait_for_healthy(&self, args: Parameters<WaitArgs>) -> ToolResult<WaitResult> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let timeout = Duration::from_secs(
            args.timeout_secs
                .unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS)
                .min(MAX_WAIT_TIMEOUT_SECS),
        );
        let mut conn = SessionConn::connect(&resolved.endpoint)
            .await
            .map_err(error_data)?;
        let conclusion = match wait_for_health(
            &resolved.endpoint,
            &mut conn,
            &args.service,
            args.after_generation,
            timeout,
        )
        .await
        {
            Ok(conclusion) => conclusion,
            Err(err) => {
                return Err(error_data(enrich_unknown_service(&args.service, err).await));
            }
        };
        Ok(Json(WaitResult::from_conclusion(
            args.service,
            timeout,
            &conclusion,
        )))
    }

    #[tool(
        description = "Wait for a service run to exit and return its exit code plus compact logs \
        produced after this call began. Intended for one-shot jobs; use wait_for_healthy for \
        servers. Pass after_generation to require a newer run. A disabled or retired service with \
        no qualifying run in flight fails immediately."
    )]
    async fn wait_for_exit(
        &self,
        args: Parameters<WaitForExitArgs>,
    ) -> ToolResult<WaitForExitResult> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let timeout = Duration::from_secs(
            args.timeout_secs
                .unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS)
                .min(MAX_WAIT_TIMEOUT_SECS),
        );
        let log_limit = args
            .log_limit
            .unwrap_or(DEFAULT_RESTART_LOG_LIMIT)
            .clamp(1, MAX_RESTART_LOG_LIMIT);
        let mut conn = SessionConn::connect(&resolved.endpoint)
            .await
            .map_err(error_data)?;
        let cursor = service_result(
            &args.service,
            current_service_cursor(&mut conn, &args.service).await,
        )
        .await?;
        let started = tokio::time::Instant::now();
        let waited = service_result(
            &args.service,
            wait_for_snapshot(
                &resolved.endpoint,
                &mut conn,
                &args.service,
                timeout,
                |snapshot| classify_exit(snapshot, args.after_generation),
            )
            .await,
        )
        .await?;
        let (status, exit_code, snapshot) = match waited {
            SnapshotWait::Matched {
                snapshot,
                verdict: ExitVerdict::Exited,
            } => (WaitForExitStatus::Exited, snapshot.last_exit_code, snapshot),
            SnapshotWait::Matched {
                snapshot,
                verdict: ExitVerdict::InvalidState,
            } => {
                let message = if let Some(reason) = snapshot.retired {
                    retired_service_message(&args.service, reason)
                } else {
                    format!(
                        "service `{}` is disabled with no qualifying run in flight",
                        args.service
                    )
                };
                return Err(error_data(ToolError::InvalidState(message)));
            }
            SnapshotWait::Timeout(snapshot) => (WaitForExitStatus::Timeout, None, snapshot),
        };
        let (logs, logs_truncated) = service_result(
            &args.service,
            compact_logs_since(&mut conn, &args.service, cursor, log_limit).await,
        )
        .await?;

        Ok(Json(WaitForExitResult {
            session_ref: SessionRef::from(&resolved.info),
            service: args.service,
            status,
            exit_code,
            run_generation: snapshot.run_generation,
            snapshot,
            logs_truncated,
            logs,
            waited_secs: started.elapsed().as_secs(),
        }))
    }

    #[tool(
        description = "Ensure a service is ready by composing session startup, explicit enabling, \
        and the generation-aware health wait. Every action taken is reported. An already-healthy \
        service returns immediately with no actions; an exit or timeout includes bounded failure \
        diagnosis. Starting a missing session requires the operator to enable session spawning. \
        Retired dynamic services must be revived with replace_dynamic_service."
    )]
    async fn ensure_service_ready(
        &self,
        args: Parameters<EnsureReadyArgs>,
    ) -> ToolResult<EnsureServiceReadyResult> {
        let Parameters(args) = args;
        let timeout = Duration::from_secs(
            args.timeout_secs
                .unwrap_or(DEFAULT_WAIT_TIMEOUT_SECS)
                .min(MAX_WAIT_TIMEOUT_SECS),
        );
        let log_limit = args
            .log_limit
            .unwrap_or(DEFAULT_DIAGNOSE_TAIL)
            .clamp(1, MAX_DIAGNOSE_TAIL);
        let mut actions = Vec::new();
        // Auto-start only for the ambient (cwd-derived) session: with an explicit selector a
        // NoSession must propagate, or the tool could silently start and then drive a session
        // for a different project than the one the caller named.
        let explicit_selector = args.session.is_some();
        let resolved = match select::resolve(&self.cwd, args.session).await {
            Ok(resolved) => resolved,
            Err(ToolError::NoSession(diagnostics)) if explicit_selector => {
                return Err(error_data(ToolError::NoSession(diagnostics)));
            }
            Err(ToolError::NoSession(_)) => {
                let target = args
                    .path
                    .as_deref()
                    .map_or_else(|| self.cwd.clone(), PathBuf::from);
                let report = self.start_for_target(target).await?;
                let id = report.id.clone().ok_or_else(|| {
                    ErrorData::internal_error(
                        "start_session did not identify the reachable session",
                        None,
                    )
                })?;
                actions.push(EnsureAction {
                    kind: EnsureActionKind::StartedSession,
                    detail: if report.started {
                        "started the micromux session".to_string()
                    } else {
                        "found the session started concurrently".to_string()
                    },
                });
                select::resolve(&self.cwd, Some(format!("hash:{id}")))
                    .await
                    .map_err(error_data)?
            }
            Err(err) => return Err(error_data(err)),
        };
        let mut conn = SessionConn::connect(&resolved.endpoint)
            .await
            .map_err(error_data)?;
        let mut snapshot = match service_snapshot(&mut conn, &args.service).await {
            Ok(snapshot) => snapshot,
            Err(err) => {
                return Err(error_data(enrich_unknown_service(&args.service, err).await));
            }
        };
        if matches!(
            convert::evaluate(&snapshot, None),
            convert::WaitOutcome::Healthy
        ) {
            return Ok(Json(EnsureServiceReadyResult {
                session_ref: SessionRef::from(&resolved.info),
                service: args.service.clone(),
                actions,
                outcome: WaitResult::healthy(args.service, snapshot.run_generation),
                snapshot,
                diagnosis: None,
            }));
        }
        if let Some(reason) = snapshot.retired {
            return Err(error_data(ToolError::InvalidState(
                retired_service_message(&args.service, reason),
            )));
        }
        let after_generation = if snapshot.desired == Desired::Disabled {
            let response = conn
                .request(Request::Enable {
                    service: args.service.clone(),
                })
                .await
                .map_err(error_data)?;
            let acks = convert::accepted(response).map_err(error_data)?;
            let generation = acks
                .iter()
                .find(|ack| ack.service == args.service)
                .map(|ack| ack.observed_generation)
                .ok_or_else(|| {
                    ErrorData::internal_error(
                        format!("enable acknowledgement omitted service `{}`", args.service),
                        None,
                    )
                })?;
            actions.push(EnsureAction {
                kind: EnsureActionKind::EnabledService,
                detail: format!("enabled service from generation {generation}"),
            });
            Some(generation)
        } else {
            None
        };
        let conclusion = match wait_for_health(
            &resolved.endpoint,
            &mut conn,
            &args.service,
            after_generation,
            timeout,
        )
        .await
        {
            Ok(conclusion) => conclusion,
            Err(err) => {
                return Err(error_data(enrich_unknown_service(&args.service, err).await));
            }
        };
        snapshot = conclusion.snapshot().clone();
        let diagnosis = if conclusion.needs_diagnosis() {
            Some(
                service_result(
                    &args.service,
                    diagnose_service(&mut conn, snapshot.clone(), log_limit).await,
                )
                .await?,
            )
        } else {
            None
        };
        actions.push(EnsureAction {
            kind: EnsureActionKind::Waited,
            detail: format!("waited for service readiness up to {}s", timeout.as_secs()),
        });
        Ok(Json(EnsureServiceReadyResult {
            session_ref: SessionRef::from(&resolved.info),
            service: args.service.clone(),
            actions,
            outcome: WaitResult::from_conclusion(args.service, timeout, &conclusion),
            snapshot,
            diagnosis,
        }))
    }
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info = Implementation::from_build_env();
        info.server_info.name = "micromux".to_string();
        info.server_info.version = env!("CARGO_PKG_VERSION").to_string();
        let mut instructions = INSTRUCTIONS.to_string();
        if !self.allow_session_start {
            instructions.push_str(
                " This server does not permit process spawning: start_session is disabled unless \
                 the operator launches `micromux mcp --allow-session-start`.",
            );
        }
        info.instructions = Some(instructions);
        info
    }
}

/// Compile an optional `grep` pattern, mapping a bad regex to an invalid-params error.
fn compile_grep(pattern: Option<&str>) -> Result<Option<Regex>, ErrorData> {
    pattern
        .map(|pattern| {
            if pattern.len() > GREP_PATTERN_MAX_BYTES {
                return Err(ErrorData::invalid_params(
                    format!("grep regex exceeds the {GREP_PATTERN_MAX_BYTES}-byte input limit"),
                    None,
                ));
            }
            RegexBuilder::new(pattern)
                .size_limit(GREP_REGEX_SIZE_LIMIT)
                .dfa_size_limit(GREP_REGEX_SIZE_LIMIT)
                .build()
                .map_err(|err| {
                    ErrorData::invalid_params(format!("invalid grep regex: {err}"), None)
                })
        })
        .transpose()
}

/// Parse an optional `min_level`, mapping an unknown level to an invalid-params error.
fn parse_min_level(level: Option<&str>) -> Result<Option<logproc::Level>, ErrorData> {
    level
        .map(|level| {
            logproc::Level::parse(level).ok_or_else(|| {
                ErrorData::invalid_params(
                    format!("unknown level `{level}` (use trace|debug|info|warn|error|fatal)"),
                    None,
                )
            })
        })
        .transpose()
}

fn parse_since(since: Option<&str>, since_unix_ms: Option<u64>) -> Result<Option<u64>, ErrorData> {
    match (since, since_unix_ms) {
        (None, None) => Ok(None),
        (None, Some(timestamp)) => Ok(Some(timestamp)),
        (Some(_), Some(_)) => Err(ErrorData::invalid_params(
            "pass either since or since_unix_ms, not both",
            None,
        )),
        (Some(raw), None) => parse_since_text(raw).map(Some),
    }
}

fn parse_since_text(raw: &str) -> Result<u64, ErrorData> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ErrorData::invalid_params("since must not be empty", None));
    }
    if let Some(duration) = parse_relative_duration(raw) {
        return Ok(unix_now_ms().saturating_sub(duration_to_millis(duration)));
    }
    if let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(raw)
        && let Ok(ms) = u64::try_from(timestamp.timestamp_millis())
    {
        return Ok(ms);
    }
    if let Ok(number) = raw.parse::<u64>() {
        return Ok(user_timestamp_to_unix_ms(number));
    }
    Err(ErrorData::invalid_params(
        "since must be RFC3339, Unix seconds/ms/us/ns, or a relative duration like 30s, 2m, 1h, 500ms",
        None,
    ))
}

fn user_timestamp_to_unix_ms(value: u64) -> u64 {
    logproc::numeric_timestamp_to_unix_ms(value).unwrap_or_else(|| value.saturating_mul(1000))
}

fn parse_relative_duration(raw: &str) -> Option<Duration> {
    const UNITS: &[(&str, u64)] = &[("ms", 1), ("s", 1_000), ("m", 60_000), ("h", 3_600_000)];

    let raw = raw.trim();
    for (suffix, multiplier) in UNITS {
        let Some(number) = raw.strip_suffix(suffix) else {
            continue;
        };
        let value = number.trim().parse::<u64>().ok()?;
        return Some(Duration::from_millis(value.checked_mul(*multiplier)?));
    }
    None
}

fn duration_to_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn unix_now_ms() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    duration_to_millis(duration)
}

fn error_data(err: ToolError) -> ErrorData {
    match err {
        ToolError::NoSession(details) => {
            let message = format!("no session: {}", details.summary);
            let data = serde_json::to_value(details).ok();
            ErrorData::invalid_params(message, data)
        }
        ToolError::Ambiguous(message) => {
            ErrorData::invalid_params(format!("ambiguous selector: {message}"), None)
        }
        ToolError::Busy(message) => {
            ErrorData::internal_error(format!("session busy: {message}"), None)
        }
        ToolError::InvalidState(message) => {
            ErrorData::invalid_params(format!("invalid state: {message}"), None)
        }
        ToolError::ConfigReload(message) => {
            ErrorData::invalid_params(format!("config reload failed: {message}"), None)
        }
        ToolError::Unsupported => ErrorData::internal_error(
            "UnsupportedPlatform: the micromux control plane is not supported on this platform",
            None,
        ),
        ToolError::Remote { code, message } => {
            ErrorData::invalid_params(format!("{code:?}: {message}"), None)
        }
        ToolError::Control(err) => ErrorData::internal_error(err.to_string(), None),
        ToolError::Unexpected(message) => ErrorData::internal_error(message, None),
    }
}

/// Serve the MCP server over stdio (JSON-RPC), blocking until the client disconnects.
///
/// # Errors
///
/// Returns an error if the stdio transport fails to initialize or the service loop errors.
pub async fn serve_stdio(
    allow_session_start: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let server = McpServer::with_session_start(allow_session_start);
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        DynamicMutationResult, ExitVerdict, GREP_PATTERN_MAX_BYTES, McpServer, MutationResult,
        RestartAndWaitResult, SessionMutationResult, SessionRef, StartDynamicAndWaitResult,
        StopSessionResult, WaitForExitResult, WaitForExitStatus, WaitResult,
        best_effort_mutation_cursor, classify_exit, compile_grep, matching_service,
        parse_since_text, session_has_service, session_selector,
    };
    use crate::select::ToolError;
    use crate::tools::health::health_attempt_matches_snapshot;
    use crate::tools::logs::{
        MergedFollowPage, follow_all_after_seq, follow_gap, merge_follow_pages, next_follow_cursor,
        truncate_wait_matches,
    };
    use color_eyre::eyre;
    use micromux::{
        Desired, Execution, HealthAttempt, LogLine, ServiceCommandAck, ServiceSnapshot,
    };
    use micromux_control::{PROTOCOL_VERSION, ServiceBrief, SessionInfo};
    use similar_asserts::assert_eq;
    use std::collections::BTreeMap;
    use std::time::Duration;

    #[test]
    fn mutations_continue_without_the_optional_log_cursor() {
        assert_eq!(
            best_effort_mutation_cursor(
                "api",
                Err(ToolError::Busy("disk log reader unavailable".to_string()))
            ),
            None
        );
        assert_eq!(best_effort_mutation_cursor("api", Ok(42)), Some(42));
    }

    #[cfg(unix)]
    use super::{
        DynamicServiceArgs, EnsureAction, EnsureActionKind, EnsureReadyArgs, LogFilterArgs,
        LogsArgs, ReconcileConfigArgs, RenewDynamicServiceArgs, ReplaceDynamicServiceArgs,
        ServiceArgs, ServiceEventsArgs, ServiceEventsResult, SessionArgs,
        SessionServiceEventsResult, StartDynamicAndWaitArgs, WaitForExitArgs, WaitStatus,
        capture_mutation_cursor,
    };

    #[cfg(unix)]
    use micromux::{DynamicServiceParams, HealthcheckSpec, Lease, PartialServiceSpec, SpecField};

    #[cfg(unix)]
    use rmcp::handler::server::wrapper::{Json, Parameters};

    #[cfg(unix)]
    use std::sync::Arc;

    #[cfg(unix)]
    #[tokio::test]
    async fn mutation_cursor_capture_is_bounded_by_its_own_timeout() -> eyre::Result<()> {
        let dir = tempfile::Builder::new()
            .prefix("micromux-mcp-cursor-")
            .tempdir_in("/tmp")?;
        let endpoint =
            micromux_control::ControlEndpoint::Unix(dir.path().join("unresponsive.sock"));
        let micromux_control::ControlEndpoint::Unix(path) = &endpoint else {
            eyre::bail!("expected Unix control endpoint");
        };
        let listener = tokio::net::UnixListener::bind(path)?;
        let peer = tokio::spawn(async move {
            // Accept the cursor request but never answer it, modelling a wedged control read.
            if let Ok((stream, _)) = listener.accept().await {
                let _stream = stream;
                std::future::pending::<()>().await;
            }
        });
        let started = tokio::time::Instant::now();

        let cursor = capture_mutation_cursor(&endpoint, "api").await;

        assert_eq!(cursor, None);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "optional cursor capture delayed the mutation path"
        );
        peer.abort();
        let _ = peer.await;
        Ok(())
    }

    #[cfg(unix)]
    struct RunningMcpSession {
        shutdown: micromux::CancellationToken,
        runner: Option<tokio::task::JoinHandle<Result<(), micromux::Error>>>,
        server: Option<tokio::task::JoinHandle<Result<(), micromux_control::ControlError>>>,
    }

    #[test]
    fn grep_regex_compilation_has_an_explicit_memory_bound() {
        assert!(compile_grep(Some("error|warn")).is_ok());

        let oversized_input = "a".repeat(GREP_PATTERN_MAX_BYTES + 1);
        assert!(compile_grep(Some(&oversized_input)).is_err());

        let oversized = "a?".repeat(100_000);
        assert!(compile_grep(Some(&oversized)).is_err());
    }

    #[cfg(unix)]
    impl RunningMcpSession {
        async fn finish(mut self) -> eyre::Result<()> {
            self.shutdown.cancel();
            if let Some(runner) = self.runner.take() {
                runner.await??;
            }
            if let Some(server) = self.server.take() {
                server.await??;
            }
            Ok(())
        }
    }

    #[cfg(unix)]
    impl Drop for RunningMcpSession {
        fn drop(&mut self) {
            self.shutdown.cancel();
        }
    }

    #[cfg(unix)]
    fn boot_dynamic_mcp_session(
        project_dir: &std::path::Path,
    ) -> eyre::Result<(RunningMcpSession, String)> {
        let yaml = r#"version: 1
control:
  dynamic_services:
    enabled: true
    allowed_working_roots: [.]
    max_services: 2
    max_lifetime: 1m
services:
  base:
    command: ["sh", "-c", "sleep 60"]
  disabled:
    command: ["sh", "-c", "sleep 60"]
    disabled: true
"#;
        boot_mcp_session_with_yaml(project_dir, "mcp-dynamic-test", yaml)
    }

    #[cfg(unix)]
    fn boot_mcp_session_with_yaml(
        project_dir: &std::path::Path,
        name: &str,
        yaml: &str,
    ) -> eyre::Result<(RunningMcpSession, String)> {
        let config_path = project_dir.join("micromux.yaml");
        std::fs::write(&config_path, yaml)?;
        let config_path = micromux_control::CanonicalConfigPath::new(config_path)?;
        let mut diagnostics = Vec::new();
        let mut config = micromux::from_str(yaml, project_dir, 0usize, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!("parse config: {err}"))?;
        config.config_path = Some(config_path.as_path().to_path_buf());

        let mux = Arc::new(micromux::Micromux::new(&config)?);
        let shutdown = micromux::CancellationToken::new();
        let (runner, handles) = mux.clone().start(shutdown.clone());
        let runner = tokio::spawn(runner);

        let runtime_dirs =
            micromux_control::usable_runtime_dirs(&micromux_control::runtime_dir_statuses());
        let runtime_dir = runtime_dirs
            .first()
            .ok_or_else(|| eyre::eyre!("no usable control runtime directory"))?;
        let endpoint = micromux_control::endpoint_for(runtime_dir, &config_path);
        let guard = micromux_control::bind(&endpoint)?
            .ok_or_else(|| eyre::eyre!("control endpoint is already owned"))?;
        let identity =
            micromux_control::SessionIdentity::new(name.to_string(), project_dir, &config_path);
        let selector = format!("hash:{}", identity.id);
        let server = Arc::new(micromux_control::ControlServer::new(
            handles.reader.clone(),
            handles.service_control(),
            identity,
            handles.dynamic_services.clone(),
        ));
        let server = tokio::spawn({
            let shutdown = shutdown.clone();
            async move { server.serve(guard, shutdown).await }
        });

        Ok((
            RunningMcpSession {
                shutdown,
                runner: Some(runner),
                server: Some(server),
            },
            selector,
        ))
    }

    #[cfg(unix)]
    fn dynamic_params_for(
        service: &str,
        command: &[&str],
        idempotency_key: Option<&str>,
    ) -> DynamicServiceParams {
        DynamicServiceParams {
            service: service.to_string(),
            spec: PartialServiceSpec {
                command: Some(command.iter().map(|part| (*part).to_string()).collect()),
                ..PartialServiceSpec::default()
            },
            from_service: None,
            extra_args: Vec::new(),
            expires_after: Some(Lease::After(Duration::from_secs(30))),
            owner: Some("mcp-integration-test".to_string()),
            idempotency_key: idempotency_key.map(str::to_string),
        }
    }

    #[cfg(unix)]
    fn dynamic_params(idempotency_key: Option<&str>) -> DynamicServiceParams {
        dynamic_params_for(
            "debug",
            &["sh", "-c", "echo dynamic-mcp-log; sleep 60"],
            idempotency_key,
        )
    }

    #[cfg(unix)]
    async fn dynamic_log_visible(server: &McpServer, selector: &str) -> eyre::Result<bool> {
        let result = server
            .get_logs(Parameters(LogsArgs {
                service: "debug".to_string(),
                session: Some(selector.to_string()),
                run_generation: None,
                tail: Some(20),
                filters: LogFilterArgs {
                    raw: false,
                    grep: None,
                    grep_context: None,
                    min_level: None,
                    since: None,
                    since_unix_ms: None,
                    trace_id: None,
                    format: crate::logproc::LogFormat::Full,
                },
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        let Json(result) = result;
        Ok(result
            .entries
            .iter()
            .any(|entry| entry.line.contains("dynamic-mcp-log")))
    }

    #[cfg(unix)]
    async fn assert_dynamic_renewal_and_events(
        server: &McpServer,
        selector: &str,
    ) -> eyre::Result<()> {
        let Json(renewed) = server
            .renew_dynamic_service(Parameters(RenewDynamicServiceArgs {
                service: "debug".to_string(),
                expected_revision: 1,
                expires_after: Some(Lease::After(Duration::from_secs(45))),
                session: Some(selector.to_string()),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        assert_eq!(renewed.receipt.revision, 1);

        let Json(events) = server
            .get_service_events(Parameters(ServiceEventsArgs {
                service: "debug".to_string(),
                session: Some(selector.to_string()),
                after_seq: None,
                after: None,
                tail: Some(20),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        let ServiceEventsResult::Single(events) = events else {
            eyre::bail!("single-service event request returned a session-wide page");
        };
        assert!(
            events
                .events
                .iter()
                .any(|event| event.kind == micromux::ServiceEventKind::Created)
        );
        assert!(
            events
                .events
                .iter()
                .any(|event| event.kind == micromux::ServiceEventKind::LeaseRenewed)
        );
        assert!(events.next_seq.is_some());
        assert!(!events.truncated);
        Ok(())
    }

    #[cfg(unix)]
    async fn start_event_test_services(server: &McpServer, selector: &str) -> eyre::Result<()> {
        for service in ["first", "second"] {
            let idempotency_key = format!("create-{service}");
            server
                .start_dynamic_service(Parameters(DynamicServiceArgs {
                    session: Some(selector.to_string()),
                    params: dynamic_params_for(
                        service,
                        &["sh", "-c", "sleep 60"],
                        Some(&idempotency_key),
                    ),
                }))
                .await
                .map_err(|err| eyre::eyre!(err.message))?;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        Ok(())
    }

    #[cfg(unix)]
    async fn renew_event_test_services(
        server: &McpServer,
        selector: &str,
        services: &[&str],
        lease_secs: u64,
    ) -> eyre::Result<()> {
        for service in services {
            server
                .renew_dynamic_service(Parameters(RenewDynamicServiceArgs {
                    service: (*service).to_string(),
                    expected_revision: 1,
                    expires_after: Some(Lease::After(Duration::from_secs(lease_secs))),
                    session: Some(selector.to_string()),
                }))
                .await
                .map_err(|err| eyre::eyre!(err.message))?;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        Ok(())
    }

    #[cfg(unix)]
    async fn wildcard_event_page(
        server: &McpServer,
        selector: &str,
        after: Option<BTreeMap<String, u64>>,
    ) -> eyre::Result<SessionServiceEventsResult> {
        let Json(page) = server
            .get_service_events(Parameters(ServiceEventsArgs {
                service: "*".to_string(),
                session: Some(selector.to_string()),
                after_seq: None,
                after,
                tail: Some(micromux::EVENT_HISTORY),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        let ServiceEventsResult::Session(page) = page else {
            eyre::bail!("wildcard event request returned a single-service page");
        };
        Ok(page)
    }

    #[cfg(unix)]
    fn assert_initial_event_page(page: &SessionServiceEventsResult) -> eyre::Result<()> {
        assert!(!page.truncated);
        assert!(page.events.windows(2).all(|events| {
            let left = &events[0];
            let right = &events[1];
            (left.event.at_unix_ms, left.service.as_str(), left.event.seq)
                <= (
                    right.event.at_unix_ms,
                    right.service.as_str(),
                    right.event.seq,
                )
        }));
        let lifecycle = page
            .events
            .iter()
            .filter(|event| {
                ["first", "second"].contains(&event.service.as_str())
                    && matches!(
                        event.event.kind,
                        micromux::ServiceEventKind::Created
                            | micromux::ServiceEventKind::LeaseRenewed
                    )
            })
            .map(|event| (event.service.as_str(), event.event.kind))
            .collect::<Vec<_>>();
        assert_eq!(
            lifecycle,
            vec![
                ("first", micromux::ServiceEventKind::Created),
                ("second", micromux::ServiceEventKind::Created),
                ("first", micromux::ServiceEventKind::LeaseRenewed),
                ("second", micromux::ServiceEventKind::LeaseRenewed),
            ]
        );
        for service in ["first", "second"] {
            let returned_cursor = page
                .events
                .iter()
                .filter(|event| event.service == service)
                .map(|event| event.event.seq)
                .max();
            assert_eq!(page.next.get(service).copied(), returned_cursor);
        }
        let encoded = serde_json::to_value(&page.events)?;
        assert!(
            encoded
                .as_array()
                .is_some_and(
                    |events| events.iter().all(|event| event.get("service").is_some()
                        && event.get("seq").is_some()
                        && event.get("event").is_none())
                )
        );
        Ok(())
    }

    #[cfg(unix)]
    fn assert_incremental_event_page(
        page: &SessionServiceEventsResult,
        previous: &BTreeMap<String, u64>,
    ) {
        assert!(
            page.events.iter().all(|event| {
                event.event.seq > previous.get(&event.service).copied().unwrap_or(0)
            })
        );
        let renewed = page
            .events
            .iter()
            .filter(|event| {
                event.event.kind == micromux::ServiceEventKind::LeaseRenewed
                    && ["first", "second"].contains(&event.service.as_str())
            })
            .map(|event| event.service.as_str())
            .collect::<Vec<_>>();
        assert_eq!(renewed, vec!["second", "first"]);
        for event in &page.events {
            assert!(
                page.next
                    .get(&event.service)
                    .is_some_and(|cursor| *cursor >= event.event.seq)
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_events_merge_and_page_with_per_service_cursors() -> eyre::Result<()> {
        let project = tempfile::tempdir()?;
        let (session, selector) = boot_dynamic_mcp_session(project.path())?;
        let mut server = McpServer::new();
        server.cwd = project.path().to_path_buf();

        start_event_test_services(&server, &selector).await?;
        renew_event_test_services(&server, &selector, &["first", "second"], 45).await?;
        let first_page = wildcard_event_page(&server, &selector, None).await?;
        assert_initial_event_page(&first_page)?;

        let previous = first_page.next;
        renew_event_test_services(&server, &selector, &["second", "first"], 50).await?;
        let next_page = wildcard_event_page(&server, &selector, Some(previous.clone())).await?;
        assert_incremental_event_page(&next_page, &previous);

        let invalid = server
            .get_service_events(Parameters(ServiceEventsArgs {
                service: "*".to_string(),
                session: Some(selector),
                after_seq: Some(1),
                after: None,
                tail: None,
            }))
            .await
            .err()
            .ok_or_else(|| eyre::eyre!("wildcard scalar cursor was accepted"))?;
        assert!(invalid.message.contains("after_seq is service-scoped"));

        session.finish().await?;
        Ok(())
    }

    #[test]
    fn server_builds_typed_tool_schemas() -> eyre::Result<()> {
        let server = McpServer::new();
        let tools = server.tool_router.list_all();
        let restart = tools
            .iter()
            .find(|tool| tool.name == "restart_service_and_wait")
            .ok_or_else(|| eyre::eyre!("missing restart_service_and_wait tool"))?;
        assert!(
            restart
                .description
                .as_deref()
                .is_some_and(|description| description.contains("hot-reload"))
        );
        let input_properties = restart
            .input_schema
            .get("properties")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| eyre::eyre!("missing input properties"))?;
        assert!(input_properties.contains_key("service"));
        assert!(input_properties.contains_key("timeout_secs"));
        assert!(input_properties.contains_key("log_limit"));

        let output_schema =
            serde_json::to_string(&schemars::schema_for!(super::RestartAndWaitResult))?;
        assert!(output_schema.contains("previous_generation"));
        assert!(output_schema.contains("run_generation"));
        assert!(output_schema.contains("snapshot"));
        assert!(output_schema.contains("diagnosis"));
        assert!(output_schema.contains("session"));
        assert!(!output_schema.contains("session_ref"));
        let ensure_schema =
            serde_json::to_string(&schemars::schema_for!(super::EnsureServiceReadyResult))?;
        assert!(ensure_schema.contains("diagnosis"));
        for name in [
            "start_dynamic_service",
            "start_dynamic_service_and_wait",
            "replace_dynamic_service",
            "renew_dynamic_service",
            "stop_dynamic_service",
            "get_service_events",
            "validate_config",
            "reconcile_config",
            "ensure_service_ready",
            "wait_for_exit",
            "get_health_history",
        ] {
            assert!(
                tools.iter().any(|tool| tool.name == name),
                "missing {name} tool"
            );
        }
        for name in ["start_dynamic_service", "replace_dynamic_service"] {
            let description = tools
                .iter()
                .find(|tool| tool.name == name)
                .and_then(|tool| tool.description.as_deref())
                .ok_or_else(|| eyre::eyre!("missing {name} description"))?;
            assert!(description.contains("from_service equal to service"));
            assert!(description.contains("extra_args re-append"));
        }
        for surface in [
            "capabilities.dynamic_services",
            "start_dynamic_service_and_wait",
            "idempotency_key",
            "renew_dynamic_service",
            "reconcile_config",
            "validate_config",
            "get_service_events",
            "ensure_service_ready",
            "wait_for_exit",
            "get_health_history",
            "micromux attach",
        ] {
            assert!(
                super::INSTRUCTIONS.contains(surface),
                "missing {surface} guidance"
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn get_health_history_returns_attempts_oldest_first() -> eyre::Result<()> {
        let project = tempfile::tempdir()?;
        let yaml = r#"version: 1
services:
  svc:
    command: ["sh", "-c", "sleep 60"]
    healthcheck:
      test: ["CMD-SHELL", "echo probe-output; exit 1"]
      start_delay: "10ms"
      interval: "50ms"
      timeout: "1s"
      retries: 50
"#;
        let (session, selector) =
            boot_mcp_session_with_yaml(project.path(), "mcp-health-history-test", yaml)?;
        let mut server = McpServer::new();
        server.cwd = project.path().to_path_buf();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let result = loop {
            let Json(result) = server
                .get_health_history(Parameters(ServiceArgs {
                    service: "svc".to_string(),
                    session: Some(selector.clone()),
                }))
                .await
                .map_err(|err| eyre::eyre!("get_health_history: {err:?}"))?;
            if result.attempts.len() >= 2 {
                break result;
            }
            if tokio::time::Instant::now() >= deadline {
                eyre::bail!("healthcheck history did not accumulate two attempts");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };

        assert_eq!(result.service, "svc");
        assert_eq!(result.snapshot.id, "svc");
        assert!(
            result
                .attempts
                .windows(2)
                .all(|pair| pair[0].attempt < pair[1].attempt)
        );
        let completed = result
            .attempts
            .iter()
            .find(|attempt| attempt.result.is_some())
            .ok_or_else(|| eyre::eyre!("no completed attempt in history"))?;
        assert!(
            completed
                .output
                .iter()
                .any(|line| line.line.contains("probe-output"))
        );
        session.finish().await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reconcile_config_round_trips_through_the_mcp_tool() -> eyre::Result<()> {
        let project = tempfile::tempdir()?;
        let (session, selector) = boot_dynamic_mcp_session(project.path())?;
        let mut server = McpServer::new();
        server.cwd = project.path().to_path_buf();
        std::fs::write(
            project.path().join("micromux.yaml"),
            r#"version: 1
control:
  dynamic_services:
    enabled: true
    allowed_working_roots: [.]
    max_services: 2
    max_lifetime: 1m
services:
  base:
    command: ["sh", "-c", "sleep 60"]
  disabled:
    command: ["sh", "-c", "sleep 60"]
    disabled: true
  added:
    command: ["sh", "-c", "sleep 60"]
"#,
        )?;

        let Json(dry_run) = server
            .reconcile_config(Parameters(ReconcileConfigArgs {
                session: Some(selector.clone()),
                dry_run: true,
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        assert!(dry_run.dry_run);
        assert!(!dry_run.session_ref.id.is_empty());
        assert_eq!(dry_run.actions.len(), 1);
        assert_eq!(dry_run.actions[0].service, "added");
        assert_eq!(
            dry_run.actions[0].action,
            micromux::ReconcileActionKind::Added
        );
        assert_json_keys(
            &dry_run,
            &["session", "id", "pid", "config_path", "dry_run", "actions"],
        )?;
        let Json(before_apply) = server
            .list_services(Parameters(SessionArgs {
                session: Some(selector.clone()),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        assert!(
            before_apply
                .services
                .iter()
                .all(|snapshot| snapshot.id != "added")
        );

        let Json(applied) = server
            .reconcile_config(Parameters(ReconcileConfigArgs {
                session: Some(selector.clone()),
                dry_run: false,
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        assert!(!applied.dry_run);
        assert_eq!(applied.actions, dry_run.actions);
        let Json(after_apply) = server
            .list_services(Parameters(SessionArgs {
                session: Some(selector),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        assert!(
            after_apply
                .services
                .iter()
                .any(|snapshot| snapshot.id == "added")
        );

        session.finish().await?;
        Ok(())
    }

    #[test]
    fn exit_wait_allows_a_disabled_run_to_finish_draining() {
        let mut snapshot = ServiceSnapshot::initial(
            "job".to_string(),
            "job".to_string(),
            Vec::new(),
            None,
            micromux::RestartPolicy::Never,
            Vec::new(),
            None,
        );
        snapshot.desired = Desired::Disabled;
        snapshot.execution = Execution::Stopping;
        snapshot.run_generation = 1;
        assert!(classify_exit(&snapshot, None).is_none());

        snapshot.execution = Execution::Exited;
        snapshot.last_exit_code = Some(3);
        assert!(matches!(
            classify_exit(&snapshot, None),
            Some(ExitVerdict::Exited)
        ));
        assert!(matches!(
            classify_exit(&snapshot, Some(1)),
            Some(ExitVerdict::InvalidState)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_dynamic_and_wait_returns_success_failure_and_replay_context() -> eyre::Result<()>
    {
        let project = tempfile::tempdir()?;
        let (session, selector) = boot_dynamic_mcp_session(project.path())?;
        let mut server = McpServer::new();
        server.cwd = project.path().to_path_buf();
        let mut healthy_params = dynamic_params_for(
            "healthy-job",
            &[
                "sh",
                "-c",
                "echo bundled-ready; touch bundle.ready; sleep 60",
            ],
            Some("bundle-healthy"),
        );
        healthy_params.spec.healthcheck = SpecField::Value(HealthcheckSpec {
            test: vec![
                "sh".to_string(),
                "-c".to_string(),
                "test -f bundle.ready".to_string(),
            ],
            interval: Duration::from_millis(10),
            timeout: Duration::from_secs(1),
            ..HealthcheckSpec::default()
        });

        let Json(healthy) = server
            .start_dynamic_service_and_wait(Parameters(StartDynamicAndWaitArgs {
                session: Some(selector.clone()),
                params: healthy_params.clone(),
                timeout_secs: Some(5),
                log_limit: Some(20),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        assert!(matches!(healthy.outcome.status, WaitStatus::Healthy));
        assert!(
            healthy
                .logs
                .iter()
                .any(|entry| entry.line.contains("bundled-ready"))
        );
        assert!(healthy.diagnosis.is_none());

        let Json(replayed) = server
            .start_dynamic_service_and_wait(Parameters(StartDynamicAndWaitArgs {
                session: Some(selector.clone()),
                params: healthy_params,
                timeout_secs: Some(5),
                log_limit: Some(20),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        assert!(replayed.receipt.idempotent_replay);
        assert!(matches!(replayed.outcome.status, WaitStatus::Healthy));

        let failing_params = dynamic_params_for(
            "failing-job",
            &["micromux-definitely-missing-command"],
            None,
        );
        let Json(failed) = server
            .start_dynamic_service_and_wait(Parameters(StartDynamicAndWaitArgs {
                session: Some(selector),
                params: failing_params,
                timeout_secs: Some(5),
                log_limit: Some(20),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        assert!(matches!(failed.outcome.status, WaitStatus::Exited));
        assert_eq!(failed.snapshot.last_exit_code, Some(-1));
        assert!(failed.diagnosis.is_some());
        assert!(
            failed
                .events
                .iter()
                .any(|event| event.kind == micromux::ServiceEventKind::SpawnFailed)
        );

        session.finish().await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn wait_for_exit_returns_job_output_times_out_and_rejects_disabled() -> eyre::Result<()> {
        let project = tempfile::tempdir()?;
        let (session, selector) = boot_dynamic_mcp_session(project.path())?;
        let mut server = McpServer::new();
        server.cwd = project.path().to_path_buf();
        let params = dynamic_params_for("job", &["sh", "-c", "sleep 0.2; echo done; exit 3"], None);
        let Json(started) = server
            .start_dynamic_service(Parameters(DynamicServiceArgs {
                session: Some(selector.clone()),
                params,
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;

        let Json(exited) = server
            .wait_for_exit(Parameters(WaitForExitArgs {
                service: "job".to_string(),
                session: Some(selector.clone()),
                after_generation: Some(started.receipt.observed_generation),
                timeout_secs: Some(5),
                log_limit: Some(20),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        assert!(matches!(exited.status, WaitForExitStatus::Exited));
        assert_eq!(exited.exit_code, Some(3));
        assert!(exited.logs.iter().any(|entry| entry.line.contains("done")));

        let Json(timed_out) = server
            .wait_for_exit(Parameters(WaitForExitArgs {
                service: "base".to_string(),
                session: Some(selector.clone()),
                after_generation: None,
                timeout_secs: Some(1),
                log_limit: Some(20),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        assert!(matches!(timed_out.status, WaitForExitStatus::Timeout));
        assert_eq!(timed_out.exit_code, None);

        let disabled = server
            .wait_for_exit(Parameters(WaitForExitArgs {
                service: "disabled".to_string(),
                session: Some(selector),
                after_generation: None,
                timeout_secs: Some(1),
                log_limit: Some(20),
            }))
            .await
            .err()
            .ok_or_else(|| eyre::eyre!("disabled service unexpectedly waited"))?;
        assert!(disabled.message.contains("disabled with no qualifying run"));

        session.finish().await?;
        Ok(())
    }

    fn session_info(id: &str, services: &[&str]) -> SessionInfo {
        SessionInfo {
            protocol_version: PROTOCOL_VERSION,
            id: id.to_string(),
            pid: 4321,
            start_time: 0,
            name: "proj".to_string(),
            working_dir: "/w".to_string(),
            config_path: "/w/micromux.yaml".to_string(),
            services: services
                .iter()
                .map(|name| ServiceBrief {
                    id: (*name).into(),
                    name: (*name).to_string(),
                })
                .collect(),
            services_truncated: false,
            micromux_version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities: None,
        }
    }

    fn test_session_ref() -> SessionRef {
        SessionRef {
            session: "proj".to_string(),
            id: "abc".to_string(),
            pid: 123,
            config_path: "/w/micromux.yaml".to_string(),
        }
    }

    fn assert_json_keys(value: &impl serde::Serialize, expected: &[&str]) -> eyre::Result<()> {
        let value = serde_json::to_value(value)?;
        let mut actual = value
            .as_object()
            .map(|object| object.keys().map(String::as_str).collect::<Vec<_>>())
            .ok_or_else(|| eyre::eyre!("receipt did not serialize as an object"))?;
        actual.sort_unstable();
        let mut expected = expected.to_vec();
        expected.sort_unstable();
        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn dynamic_capability_preflight_names_the_target_config() -> eyre::Result<()> {
        let info = session_info("h", &["svc"]);

        let error = McpServer::require_dynamic_capability(&info)
            .err()
            .ok_or_else(|| eyre::eyre!("missing capability was accepted"))?;

        assert!(error.message.contains("PolicyDenied"));
        assert!(error.message.contains("/w/micromux.yaml"));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dynamic_service_lifecycle_crosses_the_mcp_tool_layer() -> eyre::Result<()> {
        let project = tempfile::tempdir()?;
        let (session, selector) = boot_dynamic_mcp_session(project.path())?;
        let mut server = McpServer::new();
        server.cwd = project.path().to_path_buf();
        let params = dynamic_params(Some("create-debug"));

        let Json(created) = server
            .start_dynamic_service(Parameters(DynamicServiceArgs {
                session: Some(selector.clone()),
                params: params.clone(),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        assert_eq!(created.receipt.service, "debug");
        assert_eq!(created.receipt.revision, 1);
        assert!(!created.receipt.idempotent_replay);

        let Json(replayed) = server
            .start_dynamic_service(Parameters(DynamicServiceArgs {
                session: Some(selector.clone()),
                params,
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        assert!(replayed.receipt.idempotent_replay);
        assert_eq!(replayed.receipt.revision, created.receipt.revision);

        let Json(listed) = server
            .list_services(Parameters(SessionArgs {
                session: Some(selector.clone()),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        let dynamic = listed
            .services
            .iter()
            .find(|snapshot| snapshot.id == "debug")
            .ok_or_else(|| eyre::eyre!("dynamic service is missing from roster"))?;
        assert_eq!(dynamic.origin, micromux::OriginKind::Dynamic);
        assert_eq!(dynamic.dynamic.as_ref().map(|info| info.revision), Some(1));

        assert_dynamic_renewal_and_events(&server, &selector).await?;

        let stale = server
            .replace_dynamic_service(Parameters(ReplaceDynamicServiceArgs {
                expected_revision: 0,
                session: Some(selector.clone()),
                params: dynamic_params(None),
            }))
            .await
            .err()
            .ok_or_else(|| eyre::eyre!("stale replacement was accepted"))?;
        assert!(stale.message.contains("RevisionMismatch"));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !dynamic_log_visible(&server, &selector).await? {
            if tokio::time::Instant::now() >= deadline {
                eyre::bail!("dynamic service log did not become visible");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let Json(stopped) = server
            .stop_dynamic_service(Parameters(ServiceArgs {
                service: "debug".to_string(),
                session: Some(selector.clone()),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        assert!(!stopped.receipt.already_retired);

        let Json(listed) = server
            .list_services(Parameters(SessionArgs {
                session: Some(selector.clone()),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        let retired = listed
            .services
            .iter()
            .find(|snapshot| snapshot.id == "debug")
            .and_then(|snapshot| snapshot.retired);
        assert_eq!(retired, Some(micromux::RetiredReason::Stopped));
        assert!(dynamic_log_visible(&server, &selector).await?);

        session.finish().await?;
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_ready_enables_waits_short_circuits_and_enriches_misses() -> eyre::Result<()> {
        let project = tempfile::tempdir()?;
        let (session, selector) = boot_dynamic_mcp_session(project.path())?;
        let mut server = McpServer::new();
        server.cwd = project.path().to_path_buf();

        let Json(enabled) = server
            .ensure_service_ready(Parameters(EnsureReadyArgs {
                service: "disabled".to_string(),
                session: Some(selector.clone()),
                path: None,
                timeout_secs: Some(5),
                log_limit: Some(2),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        assert!(matches!(
            enabled.actions.as_slice(),
            [
                EnsureAction {
                    kind: EnsureActionKind::EnabledService,
                    ..
                },
                EnsureAction {
                    kind: EnsureActionKind::Waited,
                    ..
                }
            ]
        ));
        assert!(matches!(enabled.outcome.status, WaitStatus::Healthy));

        let Json(already_ready) = server
            .ensure_service_ready(Parameters(EnsureReadyArgs {
                service: "disabled".to_string(),
                session: Some(selector.clone()),
                path: None,
                timeout_secs: Some(5),
                log_limit: Some(2),
            }))
            .await
            .map_err(|err| eyre::eyre!(err.message))?;
        assert!(already_ready.actions.is_empty());
        assert!(matches!(already_ready.outcome.status, WaitStatus::Healthy));

        let missing = server
            .ensure_service_ready(Parameters(EnsureReadyArgs {
                service: "missing".to_string(),
                session: Some(selector),
                path: None,
                timeout_secs: Some(1),
                log_limit: Some(2),
            }))
            .await
            .err()
            .ok_or_else(|| eyre::eyre!("unknown service was accepted"))?;
        assert!(missing.message.contains("UnknownService"));
        assert!(missing.message.contains("missing"));

        session.finish().await?;
        Ok(())
    }

    #[test]
    fn session_selector_is_the_hash_form() {
        let info = session_info("b8b6127be329991b", &["rag-ui"]);
        assert_eq!(session_selector(&info), "hash:b8b6127be329991b");
    }

    #[test]
    fn session_has_service_matches_by_id_or_name_only() {
        let info = session_info("h", &["rag-ui"]);
        assert!(session_has_service(&info, "rag-ui"));
        assert!(!session_has_service(&info, "api"));
        assert!(!session_has_service(&info, "rag"));
    }

    #[test]
    fn mutation_result_serializes_flat_session_fields() -> eyre::Result<()> {
        let result = MutationResult {
            session_ref: test_session_ref(),
            accepted: vec![ServiceCommandAck {
                service: "api".to_string(),
                observed_generation: 7,
            }],
            service: "api".to_string(),
            generation: Some(7),
            log_cursor: Some(42),
        };

        let value = serde_json::to_value(result)?;
        assert_eq!(value.get("session"), Some(&serde_json::json!("proj")));
        assert_eq!(value.get("id"), Some(&serde_json::json!("abc")));
        assert_eq!(value.get("pid"), Some(&serde_json::json!(123)));
        assert_eq!(
            value.get("config_path"),
            Some(&serde_json::json!("/w/micromux.yaml"))
        );
        assert_eq!(value.get("service"), Some(&serde_json::json!("api")));
        assert_eq!(value.get("generation"), Some(&serde_json::json!(7)));
        assert_eq!(value.get("log_cursor"), Some(&serde_json::json!(42)));
        let mut keys = value
            .as_object()
            .map(|object| object.keys().map(String::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "accepted",
                "config_path",
                "generation",
                "id",
                "log_cursor",
                "pid",
                "service",
                "session"
            ]
        );
        Ok(())
    }

    #[test]
    fn session_mutation_receipts_have_exact_flat_key_sets() -> eyre::Result<()> {
        let restart_all = SessionMutationResult {
            session_ref: test_session_ref(),
            accepted: Vec::new(),
        };
        assert_json_keys(
            &restart_all,
            &["accepted", "config_path", "id", "pid", "session"],
        )?;

        let stop = StopSessionResult {
            stopped: true,
            session_ref: test_session_ref(),
            note: None,
        };
        assert_json_keys(
            &stop,
            &["config_path", "id", "note", "pid", "session", "stopped"],
        )?;
        Ok(())
    }

    #[test]
    fn dynamic_and_bundle_receipts_have_exact_flat_key_sets() -> eyre::Result<()> {
        let dynamic = DynamicMutationResult {
            session_ref: test_session_ref(),
            receipt: micromux_control::DynamicServiceAck {
                service: "debug".to_string(),
                revision: 2,
                observed_generation: 1,
                expires_at_unix_ms: Some(1234),
                command: vec!["true".to_string()],
                working_dir: Some("/w".to_string()),
                ports: Vec::new(),
                env_keys: Vec::new(),
                restart: micromux::RestartPolicy::Never,
                healthcheck_configured: false,
                already_retired: false,
                idempotent_replay: false,
            },
        };
        assert_json_keys(
            &dynamic,
            &[
                "already_retired",
                "command",
                "config_path",
                "env_keys",
                "expires_at_unix_ms",
                "healthcheck_configured",
                "id",
                "idempotent_replay",
                "observed_generation",
                "pid",
                "ports",
                "restart",
                "revision",
                "service",
                "session",
                "working_dir",
            ],
        )?;

        let snapshot = ServiceSnapshot::initial(
            "svc".to_string(),
            "svc".to_string(),
            Vec::new(),
            None,
            micromux::RestartPolicy::Never,
            Vec::new(),
            None,
        );
        let bundle = RestartAndWaitResult {
            session_ref: test_session_ref(),
            service: "svc".to_string(),
            previous_generation: 1,
            run_generation: 2,
            outcome: WaitResult::healthy("svc".to_string(), 2),
            snapshot,
            logs: Vec::new(),
            logs_truncated: false,
            events: Vec::new(),
            diagnosis: None,
        };
        assert_json_keys(
            &bundle,
            &[
                "config_path",
                "events",
                "id",
                "logs",
                "logs_truncated",
                "outcome",
                "pid",
                "previous_generation",
                "run_generation",
                "service",
                "session",
                "snapshot",
            ],
        )?;

        Ok(())
    }

    // The double-flattened bundles are exactly what these key-set pins protect: a colliding
    // field name in SessionRef, the ack, or the body would silently shadow on the wire.
    #[test]
    fn wait_bundles_have_exact_flat_key_sets() -> eyre::Result<()> {
        let start_bundle = StartDynamicAndWaitResult {
            session_ref: test_session_ref(),
            receipt: micromux_control::DynamicServiceAck {
                service: "debug".to_string(),
                revision: 1,
                observed_generation: 0,
                // None must be absent from the JSON, not null: the unbounded-lease shape.
                expires_at_unix_ms: None,
                command: vec!["true".to_string()],
                working_dir: None,
                ports: Vec::new(),
                env_keys: Vec::new(),
                restart: micromux::RestartPolicy::Never,
                healthcheck_configured: false,
                already_retired: false,
                idempotent_replay: false,
            },
            outcome: WaitResult::healthy("debug".to_string(), 1),
            snapshot: ServiceSnapshot::initial(
                "debug".to_string(),
                "debug".to_string(),
                Vec::new(),
                None,
                micromux::RestartPolicy::Never,
                Vec::new(),
                None,
            ),
            logs: Vec::new(),
            logs_truncated: false,
            events: Vec::new(),
            diagnosis: None,
        };
        assert_json_keys(
            &start_bundle,
            &[
                "already_retired",
                "command",
                "config_path",
                "env_keys",
                "events",
                "healthcheck_configured",
                "id",
                "idempotent_replay",
                "logs",
                "logs_truncated",
                "observed_generation",
                "outcome",
                "pid",
                "ports",
                "restart",
                "revision",
                "service",
                "session",
                "snapshot",
            ],
        )?;

        let exit = WaitForExitResult {
            session_ref: test_session_ref(),
            service: "job".to_string(),
            status: WaitForExitStatus::Exited,
            exit_code: Some(3),
            run_generation: 1,
            snapshot: ServiceSnapshot::initial(
                "job".to_string(),
                "job".to_string(),
                Vec::new(),
                None,
                micromux::RestartPolicy::Never,
                Vec::new(),
                None,
            ),
            logs: Vec::new(),
            logs_truncated: false,
            waited_secs: 1,
        };
        assert_json_keys(
            &exit,
            &[
                "config_path",
                "exit_code",
                "id",
                "logs",
                "logs_truncated",
                "pid",
                "run_generation",
                "service",
                "session",
                "snapshot",
                "status",
                "waited_secs",
            ],
        )?;
        Ok(())
    }

    #[test]
    fn enrichment_resolves_by_id_not_name() {
        // A service whose human name differs from its id. UnknownService enrichment must match by
        // id only (the session resolves services by id), so a session reachable only by the *name*
        // is never suggested as a retarget — otherwise the caller loops back to the same session.
        let mut info = session_info("h", &[]);
        info.services = vec![ServiceBrief {
            id: "web".into(),
            name: "frontend".to_string(),
        }];
        // find_service discovery still matches either identifier.
        assert!(session_has_service(&info, "frontend"));
        assert!(session_has_service(&info, "web"));
        // Enrichment resolves by id only.
        let snapshot = ServiceSnapshot::initial(
            "web".to_string(),
            "frontend".to_string(),
            Vec::new(),
            None,
            micromux::RestartPolicy::Never,
            Vec::new(),
            None,
        );
        assert!(
            matching_service(vec![snapshot], "frontend")
                .is_some_and(|snapshot| snapshot.id != "frontend")
        );
    }

    #[test]
    fn targeted_service_lookup_prefers_an_exact_id_over_an_earlier_name() {
        let named = ServiceSnapshot::initial(
            "api".to_string(),
            "web".to_string(),
            Vec::new(),
            None,
            micromux::RestartPolicy::Never,
            Vec::new(),
            None,
        );
        let exact = ServiceSnapshot::initial(
            "web".to_string(),
            "frontend".to_string(),
            Vec::new(),
            None,
            micromux::RestartPolicy::Never,
            Vec::new(),
            None,
        );

        let matched = matching_service(vec![named, exact], "web");

        assert_eq!(matched.map(|snapshot| snapshot.id), Some("web".to_string()));
    }

    #[test]
    fn wait_result_keeps_present_null_fields() -> eyre::Result<()> {
        let exited = serde_json::to_value(WaitResult::exited("svc".to_string(), None, 7))?;
        assert!(
            exited
                .get("exit_code")
                .is_some_and(serde_json::Value::is_null)
        );

        let mut snapshot = ServiceSnapshot::initial(
            "svc".to_string(),
            "svc".to_string(),
            Vec::new(),
            None,
            micromux::RestartPolicy::Never,
            Vec::new(),
            None,
        );
        snapshot.execution = Execution::Running;
        snapshot.run_generation = 7;
        let timeout =
            serde_json::to_value(WaitResult::timeout("svc".to_string(), 1, &snapshot, None))?;
        assert!(
            timeout
                .get("health")
                .is_some_and(serde_json::Value::is_null)
        );
        assert!(
            timeout
                .get("uptime_secs")
                .is_some_and(serde_json::Value::is_null)
        );
        assert!(
            timeout
                .get("latest_healthcheck")
                .is_some_and(serde_json::Value::is_null)
        );
        Ok(())
    }

    #[test]
    fn health_enrichment_is_current_running_generation_only() {
        let mut snapshot = ServiceSnapshot::initial(
            "svc".to_string(),
            "svc".to_string(),
            Vec::new(),
            Some(micromux::HealthcheckConfig {
                retries: 1,
                interval: Duration::from_secs(1),
                timeout: Duration::from_secs(1),
            }),
            micromux::RestartPolicy::Never,
            Vec::new(),
            None,
        );
        snapshot.execution = Execution::Running;
        snapshot.run_generation = 2;
        let mut healthcheck = HealthAttempt {
            run_generation: 2,
            attempt: 1,
            command: "curl -fsS localhost".to_string(),
            output: Vec::new(),
            result: None,
        };

        assert!(health_attempt_matches_snapshot(&snapshot, &healthcheck));

        healthcheck.run_generation = 3;
        assert!(!health_attempt_matches_snapshot(&snapshot, &healthcheck));

        healthcheck.run_generation = 2;
        snapshot.execution = Execution::Exited;
        assert!(!health_attempt_matches_snapshot(&snapshot, &healthcheck));
    }

    #[test]
    fn follow_cursor_is_last_delivered_seq_for_strict_after_protocol() {
        let lines = vec![
            LogLine {
                seq: 10,
                run_generation: 1,
                timestamp_unix_ms: 1_700_000_000_010,
                line: "a".to_string(),
            },
            LogLine {
                seq: 11,
                run_generation: 1,
                timestamp_unix_ms: 1_700_000_000_011,
                line: "b".to_string(),
            },
        ];

        assert_eq!(next_follow_cursor(&lines, Some(9)), Some(11));
        assert_eq!(next_follow_cursor(&[], Some(11)), Some(11));
        assert_eq!(next_follow_cursor(&[], None), None);
    }

    #[test]
    fn follow_gap_reports_evicted_lines() {
        let lines = vec![LogLine {
            seq: 15,
            run_generation: 2,
            timestamp_unix_ms: 1_700_000_000_015,
            line: "newest retained".to_string(),
        }];

        assert!(follow_gap(&lines, None, Some(10), None).is_some());
        assert!(follow_gap(&lines, None, Some(14), None).is_none());
        assert!(follow_gap(&[], None, Some(10), None).is_none());
        assert!(follow_gap(&[], Some(15), Some(10), None).is_some());
        assert!(follow_gap(&lines, None, None, None).is_none());
        assert!(follow_gap(&lines, None, Some(10), Some(2)).is_none());
    }

    #[test]
    fn wait_log_limit_advances_only_to_last_returned_match() {
        let mut entries = vec![
            entry("api", 11, 100),
            entry("api", 12, 101),
            entry("api", 13, 102),
        ];

        let (next_seq, truncated) = truncate_wait_matches(&mut entries, 20, false, 2);

        assert!(truncated);
        assert_eq!(next_seq, 12);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn wait_log_limit_preserves_split_record_boundary() {
        let mut entries = vec![
            entry("api", 11, 100),
            entry("api", 11, 101),
            entry("api", 12, 102),
        ];

        let (next_seq, truncated) = truncate_wait_matches(&mut entries, 20, false, 1);

        assert!(truncated);
        assert_eq!(next_seq, 11);
        assert_eq!(
            entries.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
            vec![11, 11]
        );
    }

    #[test]
    fn wait_log_split_record_without_dropped_matches_advances_to_raw_cursor() {
        let mut entries = vec![entry("api", 11, 100), entry("api", 11, 101)];

        let (next_seq, truncated) = truncate_wait_matches(&mut entries, 20, false, 1);

        assert!(!truncated);
        assert_eq!(next_seq, 20);
        assert_eq!(
            entries.iter().map(|entry| entry.seq).collect::<Vec<_>>(),
            vec![11, 11]
        );
    }

    #[test]
    fn wait_log_complete_page_advances_past_filtered_records() {
        let mut entries = vec![entry("api", 11, 100)];

        let (next_seq, truncated) = truncate_wait_matches(&mut entries, 20, false, 2);

        assert!(!truncated);
        assert_eq!(next_seq, 20);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn since_numeric_accepts_epoch_seconds() {
        assert_eq!(parse_since_text("0").unwrap(), 0);
        assert_eq!(parse_since_text("42").unwrap(), 42_000);
        assert_eq!(
            parse_since_text("1782911696789").unwrap(),
            1_782_911_696_789
        );
    }

    #[test]
    fn follow_all_cursor_map_reserves_zero_for_before_first_entry() {
        let mut cursors = BTreeMap::new();
        assert_eq!(follow_all_after_seq(&cursors, "api"), None);

        cursors.insert("api".to_string(), 12);
        assert_eq!(follow_all_after_seq(&cursors, "api"), Some(12));
        assert_eq!(follow_all_after_seq(&cursors, "worker"), Some(0));
    }

    fn entry(service: &str, seq: u64, timestamp_unix_ms: u64) -> crate::logproc::ProcessedEntry {
        crate::logproc::ProcessedEntry {
            service: Some(service.to_string()),
            seq,
            run_generation: 1,
            timestamp_unix_ms,
            source_timestamp_unix_ms: None,
            line: format!("{service}-{seq}"),
            level: None,
            message: None,
            fields: BTreeMap::new(),
        }
    }

    fn source_entry(
        service: &str,
        seq: u64,
        timestamp_unix_ms: u64,
        source_timestamp_unix_ms: u64,
    ) -> crate::logproc::ProcessedEntry {
        crate::logproc::ProcessedEntry {
            source_timestamp_unix_ms: Some(source_timestamp_unix_ms),
            ..entry(service, seq, timestamp_unix_ms)
        }
    }

    fn page(
        service: &str,
        after_seq: Option<u64>,
        raw_next_seq: Option<u64>,
        entries: Vec<crate::logproc::ProcessedEntry>,
    ) -> MergedFollowPage {
        MergedFollowPage {
            service: service.to_string(),
            after_seq,
            raw_next_seq,
            gap: None,
            entries,
            truncated: false,
        }
    }

    #[test]
    fn merged_follow_initial_tail_advances_every_service_cursor() {
        let merged = merge_follow_pages(
            vec![
                page("api", None, Some(10), vec![entry("api", 8, 300)]),
                page("worker", None, Some(7), vec![entry("worker", 7, 100)]),
                page("empty", None, None, Vec::new()),
            ],
            1,
            true,
        );

        assert!(merged.truncated);
        assert_eq!(merged.entries.len(), 1);
        assert_eq!(
            merged
                .entries
                .first()
                .and_then(|entry| entry.service.as_deref()),
            Some("api")
        );
        assert_eq!(merged.next.get("api"), Some(&10));
        assert_eq!(merged.next.get("worker"), Some(&7));
        assert_eq!(merged.next.get("empty"), Some(&0));
    }

    #[test]
    fn merged_follow_truncated_page_only_advances_returned_entries() {
        let merged = merge_follow_pages(
            vec![
                page(
                    "api",
                    Some(4),
                    Some(20),
                    vec![entry("api", 5, 100), entry("api", 6, 400)],
                ),
                page("worker", Some(9), Some(30), vec![entry("worker", 10, 200)]),
            ],
            2,
            false,
        );

        assert!(merged.truncated);
        let returned = merged
            .entries
            .iter()
            .map(|entry| (entry.service.as_deref(), entry.seq))
            .collect::<Vec<_>>();
        assert_eq!(returned, vec![(Some("api"), 5), (Some("worker"), 10)]);
        assert_eq!(merged.next.get("api"), Some(&5));
        assert_eq!(merged.next.get("worker"), Some(&10));
    }

    #[test]
    fn merged_follow_limit_preserves_split_record_boundary() {
        let merged = merge_follow_pages(
            vec![page(
                "api",
                Some(4),
                Some(6),
                vec![
                    entry("api", 5, 100),
                    entry("api", 5, 101),
                    entry("api", 6, 102),
                ],
            )],
            1,
            false,
        );

        assert!(merged.truncated);
        assert_eq!(
            merged
                .entries
                .iter()
                .map(|entry| entry.seq)
                .collect::<Vec<_>>(),
            vec![5, 5]
        );
        assert_eq!(merged.next.get("api"), Some(&5));
    }

    #[test]
    fn merged_follow_tail_limit_preserves_split_record_boundary() {
        let merged = merge_follow_pages(
            vec![page(
                "api",
                None,
                Some(6),
                vec![
                    entry("api", 5, 100),
                    entry("api", 6, 101),
                    entry("api", 6, 102),
                ],
            )],
            1,
            true,
        );

        assert!(merged.truncated);
        assert_eq!(
            merged
                .entries
                .iter()
                .map(|entry| entry.seq)
                .collect::<Vec<_>>(),
            vec![6, 6]
        );
        assert_eq!(merged.next.get("api"), Some(&6));
    }

    #[test]
    fn merged_follow_preserves_service_sequence_order_over_source_timestamps() {
        let merged = merge_follow_pages(
            vec![page(
                "api",
                Some(4),
                Some(6),
                vec![
                    source_entry("api", 5, 200, 300),
                    source_entry("api", 6, 250, 100),
                ],
            )],
            1,
            false,
        );

        assert!(merged.truncated);
        assert_eq!(merged.entries.first().map(|entry| entry.seq), Some(5));
        assert_eq!(merged.next.get("api"), Some(&5));
    }

    #[test]
    fn merged_follow_truncated_page_advances_services_with_no_matches() {
        let mut noisy = page("noisy", Some(40), Some(50), Vec::new());
        noisy.truncated = true;
        let merged = merge_follow_pages(
            vec![
                noisy,
                page("api", Some(4), Some(5), vec![entry("api", 5, 100)]),
            ],
            1,
            false,
        );

        assert!(merged.truncated);
        assert_eq!(merged.next.get("api"), Some(&5));
        assert_eq!(merged.next.get("noisy"), Some(&50));
    }

    #[test]
    fn server_truncated_page_advances_past_filtered_records_when_merged_page_is_complete() {
        let mut filtered = page("api", Some(4), Some(50), vec![entry("api", 5, 100)]);
        filtered.truncated = true;
        let merged = merge_follow_pages(vec![filtered], 10, false);

        assert!(merged.truncated);
        assert_eq!(merged.entries.len(), 1);
        assert_eq!(merged.next.get("api"), Some(&50));
    }

    #[tokio::test]
    async fn session_start_is_disabled_by_default() {
        let result = McpServer::new()
            .start_for_target(std::path::PathBuf::from("."))
            .await;

        assert!(result.is_err());
    }
}
