//! The micromux MCP server.
//!
//! A thin, stateless proxy: it discovers running micromux sessions over their local control
//! endpoints and exposes them as MCP tools. It holds no supervision state — every tool connects to
//! a session endpoint per call (cheap, local) and speaks the [`micromux_control`] protocol. All
//! actions go through the same control plane the human uses in the TUI, so dependency gating, health
//! re-probing, and restart policy are respected.

mod convert;
mod select;

use std::path::PathBuf;
use std::time::Duration;

use micromux::ChangeKind;
use micromux_control::{Client, ControlEndpoint, ErrorCode, Request, Response};
use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::convert::WaitOutcome;
use crate::select::ToolError;

const DEFAULT_WAIT_TIMEOUT_SECS: u64 = 60;
const MAX_WAIT_TIMEOUT_SECS: u64 = 600;
/// Even if a change notification is missed (the subscription isn't registered server-side until
/// after we connect, so it can drop the first one), re-poll the lossless model at least this often
/// so a quiet, healthcheck-less service that becomes healthy is never stranded until the full
/// timeout.
const WAIT_POLL_FLOOR: Duration = Duration::from_secs(1);

const INSTRUCTIONS: &str = "Discover and control running micromux sessions. \
List services, inspect current and previous run logs, restart/enable/disable services, check \
health, and wait for a service to become healthy. When no `session` is given, the tools target the \
micromux running in the current project directory. Use `list_log_runs` to find retained previous \
runs, and `follow_logs` with `next_seq` for gap-aware tailing. Actions are routed through \
micromux, so they respect dependency gating and restart policy — prefer them over `kill`+rerun. \
`restart_service`/`enable_service` return a `generation`; pass it to `wait_for_healthy` as \
`after_generation` to wait for the *new* run.";

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
struct LogsArgs {
    /// The id of the target service.
    service: String,
    /// Optional session selector; omit for the current project.
    #[serde(default)]
    session: Option<String>,
    /// Optional run generation. Omit to read the bounded visible log stream; pass a retained run
    /// generation to read a bounded tail from that disk-backed run.
    #[serde(default)]
    run_generation: Option<u64>,
    /// Number of most recent lines to return (default 200, capped by the session).
    #[serde(default)]
    tail: Option<usize>,
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
    /// Return only log lines after this cursor. Pass the `next_seq` from the previous call to
    /// resume without gaps or duplicates; omit to start from the retained history.
    #[serde(default)]
    after_seq: Option<u64>,
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

async fn send_request(endpoint: &ControlEndpoint, request: Request) -> Result<Response, ToolError> {
    let mut client = Client::connect(endpoint).await?;
    Ok(client.request(request).await?)
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
    ) -> Result<String, ErrorData> {
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
        let acks = serde_json::to_value(&acks).map_err(internal)?;
        ok_json(&json!({
            "accepted": acks,
            "service": service,
            "generation": generation,
        }))
    }
}

#[tool_router(router = tool_router)]
impl McpServer {
    #[tool(
        description = "List all micromux sessions currently running for this user, with their \
        name, pid, config path, and working directory."
    )]
    async fn list_sessions(&self) -> Result<String, ErrorData> {
        let sessions = select::list_sessions().await.map_err(error_data)?;
        let sessions = serde_json::to_value(&sessions).map_err(internal)?;
        ok_json(&json!({ "sessions": sessions }))
    }

    #[tool(
        description = "List the services in a session with their desired/execution state, health, \
        ports, uptime, restart policy, last exit code, and run generation."
    )]
    async fn list_services(&self, args: Parameters<SessionArgs>) -> Result<String, ErrorData> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let response = send_request(&resolved.endpoint, Request::ListServices)
            .await
            .map_err(error_data)?;
        let services = convert::services(response).map_err(error_data)?;
        let services = serde_json::to_value(&services).map_err(internal)?;
        ok_json(&json!({
            "config_path": resolved.info.config_path,
            "session": resolved.info.name,
            "services": services,
        }))
    }

    #[tool(
        description = "Read recent log lines for a service. Omit run_generation for the visible \
        bounded log stream; pass a retained run_generation from list_log_runs to inspect a \
        bounded tail of a current or previous disk-backed run. Use follow_logs to page through a \
        retained run with a cursor."
    )]
    async fn get_logs(&self, args: Parameters<LogsArgs>) -> Result<String, ErrorData> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let response = send_request(
            &resolved.endpoint,
            Request::GetLogs {
                service: args.service.clone(),
                run_generation: args.run_generation,
                tail: args.tail,
            },
        )
        .await
        .map_err(error_data)?;
        let logs = convert::logs(response).map_err(error_data)?;
        let entries: Vec<Value> = logs
            .lines
            .iter()
            .map(|line| {
                json!({
                    "seq": line.seq,
                    "run_generation": line.run_generation,
                    "line": line.line,
                })
            })
            .collect();
        ok_json(&json!({
            "service": args.service,
            "run_generation": args.run_generation,
            "config_path": resolved.info.config_path,
            "entries": entries,
            "truncated": logs.truncated,
        }))
    }

    #[tool(
        description = "List retained log runs for a service, including run generations and \
        sequence ranges. Use a returned run_generation with get_logs/follow_logs to inspect or \
        page through disk-backed run logs."
    )]
    async fn list_log_runs(&self, args: Parameters<ServiceArgs>) -> Result<String, ErrorData> {
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
        let runs = serde_json::to_value(&runs).map_err(internal)?;
        ok_json(&json!({
            "service": args.service,
            "config_path": resolved.info.config_path,
            "runs": runs,
        }))
    }

    #[tool(
        description = "Restart a service. Returns the run generation *before* the restart; pass \
        it to wait_for_healthy as after_generation. Restarting a disabled service is rejected."
    )]
    async fn restart_service(&self, args: Parameters<ServiceArgs>) -> Result<String, ErrorData> {
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
        description = "Restart all enabled services in a session (disabled services are skipped)."
    )]
    async fn restart_all(&self, args: Parameters<SessionArgs>) -> Result<String, ErrorData> {
        let Parameters(args) = args;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let response = send_request(&resolved.endpoint, Request::RestartAll)
            .await
            .map_err(error_data)?;
        let acks = convert::accepted(response).map_err(error_data)?;
        let acks = serde_json::to_value(&acks).map_err(internal)?;
        ok_json(&json!({ "accepted": acks }))
    }

    #[tool(
        description = "Enable (and start) a service. Returns the run generation before enabling."
    )]
    async fn enable_service(&self, args: Parameters<ServiceArgs>) -> Result<String, ErrorData> {
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
    async fn disable_service(&self, args: Parameters<ServiceArgs>) -> Result<String, ErrorData> {
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
        description = "Read log lines after a cursor for incremental following. Returns the new \
        lines and a next_seq; pass next_seq as after_seq on the next call. If retention already \
        evicted unread lines, the response includes a gap object. Pass run_generation for a full \
        disk-backed retained run page; omit it for the bounded visible stream."
    )]
    async fn follow_logs(&self, args: Parameters<FollowArgs>) -> Result<String, ErrorData> {
        let Parameters(args) = args;
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
        let next_seq = next_follow_cursor(&logs.lines, args.after_seq);
        let gap = follow_gap(&logs.lines, args.after_seq, args.run_generation);
        let entries: Vec<Value> = logs
            .lines
            .iter()
            .map(|line| {
                json!({
                    "seq": line.seq,
                    "run_generation": line.run_generation,
                    "line": line.line,
                })
            })
            .collect();
        ok_json(&json!({
            "service": args.service,
            "run_generation": args.run_generation,
            "lines": entries,
            "next_seq": next_seq,
            "gap": gap,
            "truncated": logs.truncated,
        }))
    }

    #[tool(
        description = "Show the latest healthcheck attempt for a service (command, result, output)."
    )]
    async fn get_health(&self, args: Parameters<ServiceArgs>) -> Result<String, ErrorData> {
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
        let attempt = serde_json::to_value(&attempt).map_err(internal)?;
        ok_json(&json!({ "service": args.service, "health": attempt }))
    }

    #[tool(
        description = "Wait until a service becomes healthy (or its run exits, or a timeout). \
        Pass after_generation (from restart_service/enable_service) to wait for the new run."
    )]
    async fn wait_for_healthy(&self, args: Parameters<WaitArgs>) -> Result<String, ErrorData> {
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
                    return ok_json(&json!({
                        "status": "healthy",
                        "service": args.service,
                        "generation": snapshot.run_generation,
                    }));
                }
                WaitOutcome::Exited(exit_code) => {
                    return ok_json(&json!({
                        "status": "exited",
                        "service": args.service,
                        "exit_code": exit_code,
                    }));
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
                return ok_json(&json!({
                    "status": "timeout",
                    "service": args.service,
                    "waited_secs": timeout.as_secs(),
                }));
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

fn internal<E: std::fmt::Display>(err: E) -> ErrorData {
    ErrorData::internal_error(err.to_string(), None)
}

/// Serialize a tool result to pretty JSON text. Returned as text content (no structured output
/// schema), which agents read directly.
fn ok_json(value: &Value) -> Result<String, ErrorData> {
    serde_json::to_string_pretty(value).map_err(internal)
}

fn next_follow_cursor(lines: &[micromux::LogLine], after_seq: Option<u64>) -> Option<u64> {
    lines.last().map(|line| line.seq).or(after_seq)
}

fn follow_gap(
    lines: &[micromux::LogLine],
    after_seq: Option<u64>,
    run_generation: Option<u64>,
) -> Option<Value> {
    if run_generation.is_some() {
        return None;
    }
    let after_seq = after_seq?;
    let first = lines.first()?;
    if first.seq > after_seq.saturating_add(1) {
        Some(json!({
            "after_seq": after_seq,
            "first_seq": first.seq,
            "lost_lines_at_least": first.seq.saturating_sub(after_seq).saturating_sub(1),
        }))
    } else {
        None
    }
}

fn error_data(err: ToolError) -> ErrorData {
    match err {
        ToolError::NoSession(message) => {
            ErrorData::invalid_params(format!("no session: {message}"), None)
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
    use super::{follow_gap, next_follow_cursor};
    use micromux::LogLine;
    use similar_asserts::assert_eq;

    #[test]
    fn follow_cursor_is_last_delivered_seq_for_strict_after_protocol() {
        let lines = vec![
            LogLine {
                seq: 10,
                run_generation: 1,
                line: "a".to_string(),
            },
            LogLine {
                seq: 11,
                run_generation: 1,
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
            line: "newest retained".to_string(),
        }];

        assert!(follow_gap(&lines, Some(10), None).is_some());
        assert!(follow_gap(&lines, Some(14), None).is_none());
        assert!(follow_gap(&[], Some(10), None).is_none());
        assert!(follow_gap(&lines, None, None).is_none());
        assert!(follow_gap(&lines, Some(10), Some(2)).is_none());
    }
}
