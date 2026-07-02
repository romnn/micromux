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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use micromux::{ChangeKind, Execution, Health, HealthAttempt, LogLine, ServiceSnapshot};
use micromux_control::{
    Client, ControlEndpoint, EndpointProbeResult, ErrorCode, Request, Response, SessionInfo,
    endpoint_for, endpoint_owner_lock_held, probe_endpoints, runtime_dir_statuses,
    transport_supported, unique_answering_session_probes, usable_runtime_dirs,
};
use regex::Regex;
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

use crate::convert::WaitOutcome;
use crate::select::ToolError;

type ToolResult<T> = Result<Json<T>, ErrorData>;

const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 60;
const MAX_WAIT_TIMEOUT_SECS: u64 = 600;
/// Default number of logical log entries returned by `get_logs` when the caller does not pass
/// `tail`.
const DEFAULT_LOG_TAIL: usize = 200;
/// Upper bound on entries fetched from the session per call; also the window scanned when a filter is
/// active, so `grep`/`min_level` can match against more than the returned count.
const MAX_LOG_TAIL: usize = 2000;
const DEFAULT_DIAGNOSE_TAIL: usize = 5;
const MAX_DIAGNOSE_TAIL: usize = 20;
const DIAGNOSE_LOG_SCAN: usize = 500;
/// Even if a change notification is missed (the subscription isn't registered server-side until
/// after we connect, so it can drop the first one), re-poll the lossless model at least this often
/// so a quiet, healthcheck-less service that becomes healthy is never stranded until the full
/// timeout.
const WAIT_POLL_FLOOR: Duration = Duration::from_secs(1);
const WAIT_LOG_POLL: Duration = Duration::from_millis(250);

const INSTRUCTIONS: &str = "Discover and control running micromux sessions. \
List services, inspect current and previous run logs, restart/enable/disable services, check \
health, and wait for a service to become healthy. When no `session` is given, the tools target the \
micromux running in the current project directory. Use `list_log_runs` to find retained previous \
runs, and `follow_logs` with `next_seq` for one-service tailing. Use `log_cursors` before an \
action and then `follow_all_logs` with its per-service `next` map to inspect what changed across \
services. Actions are routed through \
micromux, so they respect dependency gating and restart policy — prefer them over `kill`+rerun. \
`restart_service`/`enable_service` return a `generation`; pass it to `wait_for_healthy` as \
`after_generation` to wait for the *new* run. Use `wait_for_log` after an external action to block \
until matching backend evidence appears. Use `diagnose` for a one-shot summary of exited or \
unhealthy services, including latest healthcheck output and likely-cause log lines. Use \
`start_session`/`stop_session` to bring a \
project's services up or stop a session and free its ports (e.g. when switching git worktrees that \
bind the same ports). `get_logs`/`follow_logs`/`follow_all_logs` strip ANSI by default and accept \
a `grep` regex, `grep_context`, `since`, `trace_id`, `format=\"compact\"`, and, for services that \
emit JSON logs, a `min_level` filter.";

/// The MCP server handler. Cheap to clone; holds no supervision state.
#[derive(Clone)]
pub struct McpServer {
    cwd: PathBuf,
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
struct ServiceArgs {
    /// The id of the target service.
    service: String,
    /// Optional session selector (see `list_sessions`); omit for the current project.
    #[serde(default)]
    session: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LogFilterArgs {
    /// Keep ANSI color escapes instead of stripping them. Default false (stripped) to save tokens.
    #[serde(default)]
    raw: bool,
    /// Keep only entries matching this regex (applied after ANSI stripping).
    #[serde(default)]
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

#[derive(Serialize, JsonSchema)]
struct MutationResult {
    session: String,
    id: String,
    pid: u32,
    config_path: String,
    accepted: Vec<micromux::ServiceCommandAck>,
    service: String,
    generation: Option<u64>,
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
    services: Vec<ServiceSnapshot>,
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
    session: String,
    id: String,
    pid: u32,
    config_path: String,
    accepted: Vec<micromux::ServiceCommandAck>,
}

#[derive(Serialize, JsonSchema)]
struct StopSessionResult {
    stopped: bool,
    session: String,
    id: String,
    pid: u32,
    config_path: String,
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
struct ServiceFollowGap {
    service: String,
    after_seq: u64,
    first_seq: u64,
    lost_entries_at_least: u64,
}

#[derive(Serialize, JsonSchema)]
struct LogCursorsResult {
    config_path: String,
    cursors: BTreeMap<String, u64>,
}

#[derive(Serialize, JsonSchema)]
struct FollowGap {
    after_seq: u64,
    first_seq: u64,
    lost_entries_at_least: u64,
}

#[derive(Serialize, JsonSchema)]
struct HealthResult {
    service: String,
    health: Option<HealthAttempt>,
}

#[derive(Serialize, JsonSchema)]
struct DiagnoseResult {
    config_path: String,
    services: Vec<ServiceDiagnosis>,
}

#[derive(Serialize, JsonSchema)]
struct ServiceDiagnosis {
    service: String,
    snapshot: ServiceSnapshot,
    latest_healthcheck: Option<HealthAttempt>,
    error_log_tail: Vec<logproc::ProcessedEntry>,
    logs_truncated: bool,
    hint: String,
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

impl WaitResult {
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
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            cwd,
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
        let response = send_request(&resolved.endpoint, request)
            .await
            .map_err(error_data)?;
        let acks = convert::accepted(response).map_err(error_data)?;
        let generation = acks
            .iter()
            .find(|ack| ack.service == service)
            .map(|ack| ack.observed_generation);
        Ok(Json(MutationResult {
            session: resolved.info.name,
            id: resolved.info.id,
            pid: resolved.info.pid,
            config_path: resolved.info.config_path,
            accepted: acks,
            service: service.to_string(),
            generation,
        }))
    }
}

#[tool_router(router = tool_router)]
impl McpServer {
    #[tool(
        description = "List all micromux sessions currently running for this user, with their \
        name, pid, config path, working directory, and discovery diagnostics."
    )]
    async fn list_sessions(&self) -> ToolResult<SessionListResult> {
        let list = select::list_sessions().await.map_err(error_data)?;
        Ok(Json(SessionListResult {
            sessions: list.sessions,
            diagnostics: list.diagnostics,
        }))
    }

    #[tool(
        description = "List the services in a session with their desired/execution state, health, \
        ports, uptime, restart policy, last exit code, and run generation."
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
        Ok(Json(ServiceListResult {
            config_path: resolved.info.config_path,
            session: resolved.info.name,
            services,
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
            let response = send_request(&resolved.endpoint, Request::ListServices)
                .await
                .map_err(error_data)?;
            let services = convert::services(response).map_err(error_data)?;
            let mut entries = Vec::new();
            let mut truncated = false;
            for service in services {
                let response = send_request(
                    &resolved.endpoint,
                    Request::GetLogs {
                        service: service.id.clone(),
                        run_generation: None,
                        tail: Some(fetch_tail),
                    },
                )
                .await
                .map_err(error_data)?;
                let logs = convert::logs(response).map_err(error_data)?;
                let window_full = logs.lines.len() >= fetch_tail;
                truncated |= logs.truncated || (window_full && filtering);
                let mut shaped =
                    logproc::shape(&logs.lines, &filters.shape(args.filters.raw, None));
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
        let response = send_request(
            &resolved.endpoint,
            Request::GetLogs {
                service: args.service.clone(),
                run_generation: args.run_generation,
                tail: Some(fetch_tail),
            },
        )
        .await
        .map_err(error_data)?;
        let logs = convert::logs(response).map_err(error_data)?;
        let mut entries = logproc::shape(&logs.lines, &filters.shape(args.filters.raw, None));
        // A full fetched window that was filtered, or that yielded fewer entries than asked, may
        // hide older matches/entries beyond the scan — don't report a capped scan as complete.
        let window_full = logs.lines.len() >= fetch_tail;
        let mut truncated =
            logs.truncated || (window_full && (filtering || entries.len() < requested_tail));
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
        let runs = convert::log_runs(response).map_err(error_data)?;
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
        let runs = convert::log_runs(response).map_err(error_data)?;
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
        let cursors = current_cursors(&resolved.endpoint)
            .await
            .map_err(error_data)?;
        Ok(Json(LogCursorsResult {
            config_path: resolved.info.config_path,
            cursors,
        }))
    }

    #[tool(
        description = "Restart a service. Returns the run generation *before* the restart; pass \
        it to wait_for_healthy as after_generation. Reloads the latest micromux config before \
        spawning the replacement, so edited command flags, healthchecks, and log retention take \
        effect. Restarting a disabled service is rejected."
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
        description = "Restart all enabled services in a session (disabled services are skipped). \
        Reloads the latest micromux config before requesting restarts."
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
            session: resolved.info.name,
            id: resolved.info.id,
            pid: resolved.info.pid,
            config_path: resolved.info.config_path,
            accepted: acks,
        }))
    }

    #[tool(
        description = "Enable (and start) a service. Returns the run generation before enabling. \
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
        let response = send_request(&resolved.endpoint, Request::Shutdown)
            .await
            .map_err(error_data)?;
        convert::shutting_down(response).map_err(error_data)?;
        // Confirm the session process has exited (its supervised services were stopped) before
        // returning, so the caller can usually start another session on the same ports. A service
        // that detaches from the session's process group can still outlive it — this is not a hard
        // guarantee every port is freed.
        #[cfg(unix)]
        let stopped = wait_until_stopped(resolved.info.pid, STOP_CONFIRM_TIMEOUT).await;
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
            session: resolved.info.name,
            id: resolved.info.id,
            pid: resolved.info.pid,
            config_path: resolved.info.config_path,
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
        if !transport_supported() {
            return Err(error_data(ToolError::Unsupported));
        }
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
            return Ok(Json(report));
        }

        let mut child = spawn_detached_serve(&config_path).map_err(|err| {
            // On a platform without the control transport, surface the canonical unsupported error
            // rather than a generic spawn failure.
            if err.kind() == std::io::ErrorKind::Unsupported {
                error_data(ToolError::Unsupported)
            } else {
                ErrorData::internal_error(format!("failed to spawn `micromux serve`: {err}"), None)
            }
        })?;

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
                    return Ok(Json(start_session_report(&info, started)));
                }
                Ok(None) => {}
                Err(err) => {
                    if child_exit.is_none() {
                        let _ = child.kill().await;
                    }
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
                return Err(ErrorData::internal_error(
                    match child_exit {
                        Some(status) => format!(
                            "`micromux serve` for {} exited ({status}) before becoming reachable — \
                             run `micromux serve --config {}` to see why",
                            config_path.display(),
                            config_path.display(),
                        ),
                        None => format!(
                            "`micromux serve` for {} did not become reachable within {}s and was \
                             stopped — run `micromux serve --config {}` to diagnose",
                            config_path.display(),
                            START_READY_TIMEOUT.as_secs(),
                            config_path.display(),
                        ),
                    },
                    None,
                ));
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
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
        let logs = convert::logs(response).map_err(error_data)?;
        // Cursor + gap are computed from the raw records (seq is per record), so `next_seq` advances
        // past filtered-out entries and following never re-fetches them.
        let next_seq = next_follow_cursor(&logs.lines, args.after_seq);
        let gap = follow_gap(&logs.lines, args.after_seq, args.run_generation);
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
        let merged = follow_all_current_logs(&resolved.endpoint, &args.after, raw, &filters, limit)
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
        let deadline = tokio::time::Instant::now() + timeout;

        if args.service == "*" {
            let mut cursors = if args.after.is_empty() {
                current_cursors(&resolved.endpoint)
                    .await
                    .map_err(error_data)?
            } else {
                args.after
            };
            loop {
                let merged =
                    follow_all_current_logs(&resolved.endpoint, &cursors, raw, &filters, limit)
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
            None => current_service_cursor(&resolved.endpoint, &args.service)
                .await
                .map_err(error_data)?,
        };
        loop {
            let response = send_request(
                &resolved.endpoint,
                Request::FollowLogs {
                    service: args.service.clone(),
                    run_generation: None,
                    after: Some(cursor),
                },
            )
            .await
            .map_err(error_data)?;
            let logs = convert::logs(response).map_err(error_data)?;
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
        description = "Show the latest healthcheck attempt for a service (command, result, output)."
    )]
    async fn get_health(&self, args: Parameters<ServiceArgs>) -> ToolResult<HealthResult> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let response = send_request(
            &resolved.endpoint,
            Request::GetHealth {
                service: args.service.clone(),
            },
        )
        .await
        .map_err(error_data)?;
        let attempt = convert::health(response).map_err(error_data)?;
        Ok(Json(HealthResult {
            service: args.service,
            health: attempt,
        }))
    }

    #[tool(
        description = "Summarize services that need attention in one call. Returns exited, \
        starting/pending/stopping, or unhealthy services with their full state snapshot, latest \
        healthcheck output, and a compact tail of likely-cause log lines."
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
        let response = send_request(&resolved.endpoint, Request::ListServices)
            .await
            .map_err(error_data)?;
        let services = convert::services(response).map_err(error_data)?;
        let mut diagnoses = Vec::new();
        for snapshot in services.into_iter().filter(service_needs_diagnosis) {
            let latest_healthcheck = latest_health(&resolved.endpoint, &snapshot.id)
                .await
                .map(bounded_attempt);
            let (error_log_tail, logs_truncated) =
                likely_cause_log_tail(&resolved.endpoint, &snapshot, tail)
                    .await
                    .map_err(error_data)?;
            let hint = diagnosis_hint(&snapshot, latest_healthcheck.as_ref(), &error_log_tail);
            diagnoses.push(ServiceDiagnosis {
                service: snapshot.id.clone(),
                snapshot,
                latest_healthcheck,
                error_log_tail,
                logs_truncated,
                hint,
            });
        }
        Ok(Json(DiagnoseResult {
            config_path: resolved.info.config_path,
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
        let deadline = tokio::time::Instant::now() + timeout;
        // Best-effort wakeup; `None` (subscribe failed or stream ended) degrades to pure polling.
        let mut subscription = Client::subscribe(&resolved.endpoint).await.ok();

        loop {
            let response = send_request(&resolved.endpoint, Request::ListServices)
                .await
                .map_err(error_data)?;
            let services = convert::services(response).map_err(error_data)?;
            let snapshot = services
                .into_iter()
                .find(|snapshot| snapshot.id == args.service)
                .ok_or_else(|| {
                    error_data(ToolError::Remote {
                        code: ErrorCode::UnknownService,
                        message: format!("unknown service `{}`", args.service),
                    })
                })?;

            match convert::evaluate(&snapshot, args.after_generation) {
                WaitOutcome::Healthy => {
                    return Ok(Json(WaitResult::healthy(
                        args.service,
                        snapshot.run_generation,
                    )));
                }
                WaitOutcome::Exited(exit_code) => {
                    return Ok(Json(WaitResult::exited(
                        args.service,
                        exit_code,
                        snapshot.run_generation,
                    )));
                }
                WaitOutcome::InvalidState => {
                    return Err(error_data(ToolError::InvalidState(format!(
                        "service `{}` is disabled and will not become healthy",
                        args.service
                    ))));
                }
                WaitOutcome::Pending => {}
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                // Surface the facts we have rather than guessing "still building": the execution
                // sub-state and the latest healthcheck attempt distinguish "process up, probe
                // failing" from "still starting" without a heuristic.
                let latest = latest_health(&resolved.endpoint, &args.service)
                    .await
                    .map(bounded_attempt);
                return Ok(Json(WaitResult::timeout(
                    args.service,
                    timeout.as_secs(),
                    &snapshot,
                    latest,
                )));
            }
            let wait = deadline.saturating_duration_since(now).min(WAIT_POLL_FLOOR);

            // Wake on a relevant change (this service; ignore log appends, which don't affect
            // health), but never wait longer than the poll floor before re-polling. Drop the
            // subscription if its stream ends — polling still converges.
            let drop_subscription = if let Some(stream) = subscription.as_mut() {
                let relevant = tokio::time::timeout(wait, async {
                    loop {
                        match stream.recv().await {
                            Ok(Some(change))
                                if change.service_id == args.service
                                    && change.kind != ChangeKind::Logs =>
                            {
                                return true;
                            }
                            Ok(Some(_)) => {}
                            Ok(None) | Err(_) => return false,
                        }
                    }
                })
                .await;
                matches!(relevant, Ok(false))
            } else {
                tokio::time::sleep(wait).await;
                false
            };
            if drop_subscription {
                subscription = None;
            }
        }
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
        info.instructions = Some(INSTRUCTIONS.to_string());
        info
    }
}

/// Compile an optional `grep` pattern, mapping a bad regex to an invalid-params error.
fn compile_grep(pattern: Option<&str>) -> Result<Option<Regex>, ErrorData> {
    pattern
        .map(|pattern| {
            Regex::new(pattern).map_err(|err| {
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

/// Best-effort fetch of the latest healthcheck attempt, used only to enrich a timeout report. Any
/// error degrades to `None` — the report is still useful without it.
async fn latest_health(endpoint: &ControlEndpoint, service: &str) -> Option<HealthAttempt> {
    let response = send_request(
        endpoint,
        Request::GetHealth {
            service: service.to_string(),
        },
    )
    .await
    .ok()?;
    convert::health(response).ok().flatten()
}

/// Trim a healthcheck attempt's captured output to its last lines, so a chatty probe doesn't bloat
/// the timeout report.
fn bounded_attempt(mut attempt: HealthAttempt) -> HealthAttempt {
    const MAX_OUTPUT_LINES: usize = 20;
    if attempt.output.len() > MAX_OUTPUT_LINES {
        let drop = attempt.output.len() - MAX_OUTPUT_LINES;
        attempt.output.drain(0..drop);
    }
    attempt
}

fn service_needs_diagnosis(snapshot: &ServiceSnapshot) -> bool {
    if snapshot.desired == micromux::Desired::Disabled {
        return false;
    }
    snapshot.execution != Execution::Running
        || (snapshot.healthcheck_configured && snapshot.health != Some(Health::Healthy))
}

async fn likely_cause_log_tail(
    endpoint: &ControlEndpoint,
    snapshot: &ServiceSnapshot,
    limit: usize,
) -> Result<(Vec<logproc::ProcessedEntry>, bool), ToolError> {
    let logs = logs_for_diagnosis(endpoint, snapshot).await?;
    let mut entries = logproc::shape(
        &logs.lines,
        &logproc::Shape {
            format: logproc::LogFormat::Compact,
            ..logproc::Shape::default()
        },
    )
    .into_iter()
    .filter(is_likely_cause_entry)
    .collect::<Vec<_>>();
    let mut truncated = logs.truncated;
    if entries.len() > limit {
        truncated |= logproc::tail_preserving_record_boundaries(&mut entries, limit);
    }
    Ok((entries, truncated))
}

async fn logs_for_diagnosis(
    endpoint: &ControlEndpoint,
    snapshot: &ServiceSnapshot,
) -> Result<convert::LogsResult, ToolError> {
    if snapshot.run_generation > 0 {
        match fetch_log_tail(endpoint, &snapshot.id, Some(snapshot.run_generation)).await {
            Ok(logs) => return Ok(logs),
            Err(ToolError::Remote {
                code: ErrorCode::UnknownRun,
                ..
            }) => {}
            Err(err) => return Err(err),
        }
    }
    fetch_log_tail(endpoint, &snapshot.id, None).await
}

async fn fetch_log_tail(
    endpoint: &ControlEndpoint,
    service: &str,
    run_generation: Option<u64>,
) -> Result<convert::LogsResult, ToolError> {
    let response = send_request(
        endpoint,
        Request::GetLogs {
            service: service.to_string(),
            run_generation,
            tail: Some(DIAGNOSE_LOG_SCAN),
        },
    )
    .await?;
    convert::logs(response)
}

fn is_likely_cause_entry(entry: &logproc::ProcessedEntry) -> bool {
    if matches!(entry.level, Some("error" | "fatal")) {
        return true;
    }
    let lower = entry.line.to_ascii_lowercase();
    lower.contains("[stderr]")
        || lower.contains("error")
        || lower.contains("failed")
        || lower.contains("panic")
        || lower.contains("exception")
        || lower.contains("traceback")
        || lower.contains("does not exist")
}

fn diagnosis_hint(
    snapshot: &ServiceSnapshot,
    latest_healthcheck: Option<&HealthAttempt>,
    error_log_tail: &[logproc::ProcessedEntry],
) -> String {
    match snapshot.execution {
        Execution::Exited => {
            if error_log_tail.is_empty() {
                "service exited; inspect get_logs for the full retained run".to_string()
            } else {
                "service exited; error_log_tail contains likely cause lines".to_string()
            }
        }
        Execution::Running if snapshot.healthcheck_configured => {
            if latest_healthcheck.is_some() {
                "service is running but healthcheck is not healthy; inspect latest_healthcheck output"
                    .to_string()
            } else {
                "service is running but healthcheck is not healthy yet; no healthcheck attempt has completed"
                    .to_string()
            }
        }
        Execution::Pending | Execution::Starting => {
            "service has not finished starting; inspect latest_healthcheck and recent logs"
                .to_string()
        }
        Execution::Stopping => "service is stopping".to_string(),
        Execution::Running => {
            "service state needs attention; inspect snapshot and logs".to_string()
        }
    }
}

async fn current_service_cursor(
    endpoint: &ControlEndpoint,
    service: &str,
) -> Result<u64, ToolError> {
    let response = send_request(
        endpoint,
        Request::ListLogRuns {
            service: service.to_string(),
        },
    )
    .await?;
    let cursor = convert::log_runs(response)?
        .into_iter()
        .rev()
        .find(|run| run.current)
        .and_then(|run| run.last_seq);
    if let Some(cursor) = cursor {
        return Ok(cursor);
    }

    let response = send_request(
        endpoint,
        Request::GetLogs {
            service: service.to_string(),
            run_generation: None,
            tail: Some(1),
        },
    )
    .await?;
    Ok(convert::logs(response)?
        .lines
        .last()
        .map_or(0, |line| line.seq))
}

async fn current_cursors(endpoint: &ControlEndpoint) -> Result<BTreeMap<String, u64>, ToolError> {
    let response = send_request(endpoint, Request::ListServices).await?;
    let services = convert::services(response)?;
    let mut cursors = BTreeMap::new();
    for service in services {
        cursors.insert(
            service.id.clone(),
            current_service_cursor(endpoint, &service.id).await?,
        );
    }
    Ok(cursors)
}

async fn follow_all_current_logs(
    endpoint: &ControlEndpoint,
    after: &BTreeMap<String, u64>,
    raw: bool,
    filters: &LogFilters,
    limit: usize,
) -> Result<MergedFollowOutput, ToolError> {
    let tail_mode = after.is_empty();
    let response = send_request(endpoint, Request::ListServices).await?;
    let services = convert::services(response)?;
    let mut pages = Vec::new();
    for service in services {
        let after_seq = follow_all_after_seq(after, &service.id);
        let response = send_request(
            endpoint,
            Request::FollowLogs {
                service: service.id.clone(),
                run_generation: None,
                after: after_seq,
            },
        )
        .await?;
        let logs = convert::logs(response)?;
        let raw_next_seq = next_follow_cursor(&logs.lines, after_seq);
        let gap = follow_gap(&logs.lines, after_seq, None);
        let mut entries = logproc::shape(&logs.lines, &filters.shape(raw, None));
        for entry in &mut entries {
            entry.service = Some(service.id.clone());
        }
        pages.push(MergedFollowPage {
            service: service.id,
            after_seq,
            raw_next_seq,
            gap,
            entries,
            truncated: logs.truncated,
        });
    }
    Ok(merge_follow_pages(pages, limit, tail_mode))
}

/// A factual next-step hint for a `wait_for_healthy` timeout, derived from the execution sub-state.
fn timeout_hint(snapshot: &ServiceSnapshot) -> &'static str {
    match snapshot.execution {
        Execution::Running => {
            if snapshot.healthcheck_configured && snapshot.health != Some(Health::Healthy) {
                "the process is running but its healthcheck has not passed yet — inspect \
                 latest_healthcheck (or call get_health); if the command is still compiling/starting, \
                 wait again with a longer timeout_secs"
            } else {
                "the process is running and may still be completing startup — wait again or inspect \
                 get_logs"
            }
        }
        Execution::Pending | Execution::Starting => {
            "the process has not finished starting — wait again with a longer timeout_secs or inspect \
             get_logs"
        }
        Execution::Stopping => "the service is stopping",
        Execution::Exited => {
            "the run has exited — inspect get_logs and the service's last_exit_code"
        }
    }
}

/// How long `stop_session` waits to confirm the session process actually exited.
#[cfg(unix)]
const STOP_CONFIRM_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether a process is still alive, via a signal-0 `kill`. Used to confirm a stopped session exited
/// (so its ports are freed) before reporting success.
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // Reject pid 0: kill(0, …) targets the caller's own process group, never a session process.
    if pid == 0 {
        return false;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // Signal 0 delivers nothing; it only checks existence/permission. ESRCH means the process is
    // gone; anything else (Ok, or EPERM) means it still exists.
    !matches!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

/// Poll until `pid` is gone or the timeout elapses; returns whether it exited.
#[cfg(unix)]
async fn wait_until_stopped(pid: u32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !process_alive(pid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// How long `start_session` waits for a freshly spawned session's control plane to come up.
const START_READY_TIMEOUT: Duration = Duration::from_secs(15);
/// After a spawned `serve` exits (e.g. it lost the lifetime-lock race to a concurrent start), how
/// long to keep polling for a session to become reachable before reporting failure — long enough for
/// the race winner (which binds within milliseconds) to come up, short enough to fail a bad config
/// quickly.
const CHILD_EXIT_GRACE: Duration = Duration::from_secs(3);

async fn already_running_any(
    endpoints: &[ControlEndpoint],
    config_path: &Path,
) -> Result<Option<StartSessionResult>, ToolError> {
    let probes = probe_endpoints(endpoints).await;
    let sessions = unique_answering_session_probes(&probes);
    if sessions.len() > 1 {
        return Err(ambiguous_start_session(config_path, sessions.len()));
    }
    if let Some((_endpoint, info)) = sessions.into_iter().next() {
        return Ok(Some(already_running_report(&info)));
    }

    for probe in probes {
        let EndpointProbeResult::Unreachable(reason) = probe.result else {
            continue;
        };
        if endpoint_owner_lock_held(&probe.endpoint).unwrap_or(false) {
            return Ok(Some(StartSessionResult {
                started: false,
                already_running: Some(true),
                reachable: Some(false),
                config_path: config_path.display().to_string(),
                session: None,
                id: None,
                pid: None,
                endpoint: Some(probe.endpoint.to_string()),
                reason: Some(reason),
            }));
        }
    }

    Ok(None)
}

async fn reachable_session_for_start(
    endpoints: &[ControlEndpoint],
    config_path: &Path,
    child_pid: Option<u32>,
) -> Result<Option<(bool, SessionInfo)>, ToolError> {
    let probes = probe_endpoints(endpoints).await;
    let sessions = unique_answering_session_probes(&probes);
    if sessions.len() > 1 {
        return Err(ambiguous_start_session(config_path, sessions.len()));
    }
    let Some((_endpoint, info)) = sessions.into_iter().next() else {
        return Ok(None);
    };
    Ok(Some((child_pid == Some(info.pid), info)))
}

fn start_session_report(info: &SessionInfo, started: bool) -> StartSessionResult {
    if started {
        StartSessionResult {
            started: true,
            already_running: None,
            reachable: None,
            config_path: info.config_path.clone(),
            session: Some(info.name.clone()),
            id: Some(info.id.clone()),
            pid: Some(info.pid),
            endpoint: None,
            reason: None,
        }
    } else {
        already_running_report(info)
    }
}

fn already_running_report(info: &SessionInfo) -> StartSessionResult {
    StartSessionResult {
        started: false,
        already_running: Some(true),
        reachable: None,
        config_path: info.config_path.clone(),
        session: Some(info.name.clone()),
        id: Some(info.id.clone()),
        pid: Some(info.pid),
        endpoint: None,
        reason: None,
    }
}

fn ambiguous_start_session(config_path: &Path, sessions: usize) -> ToolError {
    ToolError::Ambiguous(format!(
        "config {} matched {sessions} live sessions; use list_sessions and stop the duplicate before starting another",
        config_path.display(),
    ))
}

/// Spawn `micromux serve` detached for a project's config and return its child handle. A new process
/// group + null stdio detach it from this ephemeral MCP server and keep it off the JSON-RPC stdio
/// channel; `--config` pins the same endpoint hash the proxy derives, regardless of the child's
/// working directory. Using `tokio::process` means the runtime reaps the child in the background once
/// it exits (no zombie), so a later `kill(pid, 0)` reports it truly gone, and lets `start_session`
/// observe an early exit.
#[cfg(unix)]
fn spawn_detached_serve(config_path: &Path) -> std::io::Result<tokio::process::Child> {
    use std::process::Stdio;

    let exe = std::env::current_exe()?;
    let project_dir = config_path.parent().unwrap_or(config_path);
    tokio::process::Command::new(exe)
        .arg("serve")
        .arg("--config")
        .arg(config_path)
        .current_dir(project_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
}

#[cfg(not(unix))]
fn spawn_detached_serve(_config_path: &Path) -> std::io::Result<tokio::process::Child> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "start_session is only supported on unix",
    ))
}

fn next_follow_cursor(lines: &[LogLine], after_seq: Option<u64>) -> Option<u64> {
    lines.last().map(|line| line.seq).or(after_seq)
}

fn truncate_wait_matches(
    entries: &mut Vec<logproc::ProcessedEntry>,
    raw_next_seq: u64,
    server_truncated: bool,
    limit: usize,
) -> (u64, bool) {
    if entries.len() <= limit {
        return (raw_next_seq, server_truncated);
    }
    let truncated = logproc::truncate_preserving_record_boundaries(entries, limit);
    let next_seq = if truncated {
        entries.last().map_or(raw_next_seq, |entry| entry.seq)
    } else {
        raw_next_seq
    };
    (next_seq, server_truncated || truncated)
}

fn follow_gap(
    lines: &[LogLine],
    after_seq: Option<u64>,
    run_generation: Option<u64>,
) -> Option<FollowGap> {
    if run_generation.is_some() {
        return None;
    }
    let after_seq = after_seq?;
    let first = lines.first()?;
    if first.seq > after_seq.saturating_add(1) {
        Some(FollowGap {
            after_seq,
            first_seq: first.seq,
            lost_entries_at_least: first.seq.saturating_sub(after_seq).saturating_sub(1),
        })
    } else {
        None
    }
}

fn follow_all_after_seq(cursors: &BTreeMap<String, u64>, service_id: &str) -> Option<u64> {
    if cursors.is_empty() {
        None
    } else {
        Some(cursors.get(service_id).copied().unwrap_or(0))
    }
}

struct MergedFollowPage {
    service: String,
    after_seq: Option<u64>,
    raw_next_seq: Option<u64>,
    gap: Option<FollowGap>,
    entries: Vec<logproc::ProcessedEntry>,
    truncated: bool,
}

struct MergedFollowOutput {
    entries: Vec<logproc::ProcessedEntry>,
    next: BTreeMap<String, u64>,
    gaps: Vec<ServiceFollowGap>,
    truncated: bool,
}

fn merge_follow_pages(
    mut pages: Vec<MergedFollowPage>,
    limit: usize,
    tail_mode: bool,
) -> MergedFollowOutput {
    let page_truncated = pages.iter().any(|page| page.truncated);
    let matched_by_service = pages
        .iter()
        .map(|page| (page.service.clone(), !page.entries.is_empty()))
        .collect::<BTreeMap<_, _>>();
    let entries = pages
        .iter_mut()
        .flat_map(|page| std::mem::take(&mut page.entries))
        .collect::<Vec<_>>();
    let mut entries = logproc::merge_preserving_service_order(entries);
    let mut merge_truncated = false;
    if entries.len() > limit {
        merge_truncated = if tail_mode {
            logproc::tail_preserving_record_boundaries(&mut entries, limit)
        } else {
            logproc::truncate_preserving_record_boundaries(&mut entries, limit)
        };
    }

    let returned = returned_cursors(&entries);
    let mut next = BTreeMap::new();
    for page in &pages {
        let cursor = if merge_truncated && !tail_mode {
            returned
                .get(&page.service)
                .copied()
                .or_else(|| {
                    matched_by_service
                        .get(&page.service)
                        .is_some_and(|matched| !matched)
                        .then_some(page.raw_next_seq)
                        .flatten()
                })
                .or(page.after_seq)
        } else {
            page.raw_next_seq
                .or(page.after_seq)
                .or(tail_mode.then_some(0))
        };
        if let Some(cursor) = cursor {
            next.insert(page.service.clone(), cursor);
        }
    }
    let gaps = pages
        .into_iter()
        .filter_map(|page| {
            page.gap.map(|gap| ServiceFollowGap {
                service: page.service,
                after_seq: gap.after_seq,
                first_seq: gap.first_seq,
                lost_entries_at_least: gap.lost_entries_at_least,
            })
        })
        .collect();

    MergedFollowOutput {
        entries,
        next,
        gaps,
        truncated: page_truncated || merge_truncated,
    }
}

fn returned_cursors(entries: &[logproc::ProcessedEntry]) -> BTreeMap<String, u64> {
    let mut cursors: BTreeMap<String, u64> = BTreeMap::new();
    for entry in entries {
        if let Some(service) = &entry.service {
            cursors
                .entry(service.clone())
                .and_modify(|seq| *seq = (*seq).max(entry.seq))
                .or_insert(entry.seq);
        }
    }
    cursors
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
pub async fn serve_stdio() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let server = McpServer::new();
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        McpServer, MergedFollowPage, WaitResult, follow_all_after_seq, follow_gap,
        merge_follow_pages, next_follow_cursor, parse_since_text, truncate_wait_matches,
    };
    use micromux::{Execution, LogLine, ServiceSnapshot};
    use similar_asserts::assert_eq;
    use std::collections::BTreeMap;

    #[test]
    fn server_builds_typed_tool_schemas() {
        let _ = McpServer::new();
    }

    #[test]
    fn wait_result_keeps_present_null_fields() -> color_eyre::Result<()> {
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
            false,
            micromux::RestartPolicy::Never,
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

        assert!(follow_gap(&lines, Some(10), None).is_some());
        assert!(follow_gap(&lines, Some(14), None).is_none());
        assert!(follow_gap(&[], Some(10), None).is_none());
        assert!(follow_gap(&lines, None, None).is_none());
        assert!(follow_gap(&lines, Some(10), Some(2)).is_none());
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
}
