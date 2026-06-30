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

use std::path::{Path, PathBuf};
use std::time::Duration;

use micromux::{ChangeKind, Execution, Health, HealthAttempt, ServiceSnapshot};
use micromux_control::{
    Client, ControlEndpoint, ErrorCode, Request, Response, SessionInfo, endpoint_for, runtime_dir,
};
use regex::Regex;
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
/// Default number of visual lines returned by `get_logs` when the caller does not pass `tail`.
const DEFAULT_LOG_TAIL: usize = 200;
/// Upper bound on lines fetched from the session per call; also the window scanned when a filter is
/// active, so `grep`/`min_level` can match against more than the returned count.
const MAX_LOG_TAIL: usize = 2000;
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
`after_generation` to wait for the *new* run. Use `start_session`/`stop_session` to bring a \
project's services up or stop a session and free its ports (e.g. when switching git worktrees that \
bind the same ports). `get_logs`/`follow_logs` strip ANSI by default and accept a `grep` regex and, \
for services that emit JSON logs, a `min_level` filter.";

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
    /// Number of most recent visual lines to return (default 200, capped at 2000).
    #[serde(default)]
    tail: Option<usize>,
    /// Keep ANSI color escapes instead of stripping them. Default false (stripped) to save tokens.
    #[serde(default)]
    raw: Option<bool>,
    /// Keep only lines matching this regex (applied after ANSI stripping).
    #[serde(default)]
    grep: Option<String>,
    /// Keep only structured-JSON log lines at or above this level
    /// (`trace`<`debug`<`info`<`warn`<`error`<`fatal`). Lines that are not JSON with a level field
    /// are dropped when this is set, so only use it for services that emit JSON logs.
    #[serde(default)]
    min_level: Option<String>,
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
    /// Keep ANSI color escapes instead of stripping them. Default false (stripped) to save tokens.
    #[serde(default)]
    raw: Option<bool>,
    /// Keep only lines matching this regex (applied after ANSI stripping). `next_seq` still advances
    /// past filtered-out lines, so following never re-fetches them.
    #[serde(default)]
    grep: Option<String>,
    /// Keep only structured-JSON log lines at or above this level
    /// (`trace`<`debug`<`info`<`warn`<`error`<`fatal`). Lines that are not JSON with a level field
    /// are dropped when this is set.
    #[serde(default)]
    min_level: Option<String>,
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
        retained run with a cursor. ANSI color is stripped by default (raw=true keeps it) and tail \
        counts visual lines. Filter with grep (regex) or, for JSON-log services, min_level; entries \
        carry a detected `level` when the line is structured JSON."
    )]
    async fn get_logs(&self, args: Parameters<LogsArgs>) -> Result<String, ErrorData> {
        let Parameters(args) = args;
        let grep = compile_grep(args.grep.as_deref())?;
        let min_level = parse_min_level(args.min_level.as_deref())?;
        let resolved = select::resolve(&self.cwd, args.session)
            .await
            .map_err(error_data)?;
        let filtering = grep.is_some() || min_level.is_some();
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
        let entries = logproc::shape(
            &logs.lines,
            &logproc::Shape {
                raw: args.raw.unwrap_or(false),
                grep: grep.as_ref(),
                min_level,
                limit: Some(requested_tail),
            },
        );
        // A full fetched window that was filtered, or that yielded fewer visual lines than asked,
        // may hide older matches/lines beyond the scan — don't report a capped scan as complete.
        let window_full = logs.lines.len() >= fetch_tail;
        let truncated =
            logs.truncated || (window_full && (filtering || entries.len() < requested_tail));
        let entries = serde_json::to_value(&entries).map_err(internal)?;
        ok_json(&json!({
            "service": args.service,
            "run_generation": args.run_generation,
            "config_path": resolved.info.config_path,
            "entries": entries,
            "truncated": truncated,
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
        description = "Stop a whole micromux session: every service is stopped and the session \
        process exits (graceful, like Ctrl-C), freeing its ports. Select with `session` (see \
        list_sessions); omit for the current project. Use this before start_session for another \
        worktree that binds the same ports. Returns `stopped: true` once the process has exited."
    )]
    async fn stop_session(&self, args: Parameters<SessionArgs>) -> Result<String, ErrorData> {
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
        ok_json(&json!({
            "stopped": stopped,
            "session": resolved.info.name,
            "pid": resolved.info.pid,
            "config_path": resolved.info.config_path,
            "note": note,
        }))
    }

    #[tool(
        description = "Start a new headless micromux session for a project (brings its services \
        up). Spawns `micromux serve` detached for the project's config and returns once the session \
        is reachable; a no-op if one is already running for that config. `path` is a project \
        directory or a config file — omit for the MCP server's directory. If another worktree's \
        session binds the same ports, stop it first with stop_session."
    )]
    async fn start_session(&self, args: Parameters<StartArgs>) -> Result<String, ErrorData> {
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
        let runtime_dir = runtime_dir().ok_or_else(|| {
            ErrorData::internal_error("no runtime directory could be resolved", None)
        })?;
        let endpoint = endpoint_for(&runtime_dir, &config_path);

        // Resolve before spawning: never double-start. Anything listening on the endpoint — even a
        // busy or version-mismatched session that won't answer Describe — already owns it.
        if let Some(report) = already_running(&endpoint).await {
            return ok_json(&report);
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
            if let Some(info) = describe(&endpoint).await {
                // Reachable. If our own child is still alive, it is the one that came up. If it had
                // already exited, a *concurrent* start_session won the endpoint lock and is coming up
                // instead — report that honestly rather than as our start. Dropping `child` hands any
                // survivor to tokio's background reaper, so it never zombies.
                let report = if child_exit.is_some() {
                    json!({
                        "started": false,
                        "already_running": true,
                        "session": info.name,
                        "pid": info.pid,
                        "config_path": info.config_path,
                    })
                } else {
                    json!({
                        "started": true,
                        "session": info.name,
                        "pid": info.pid,
                        "config_path": info.config_path,
                    })
                };
                return ok_json(&report);
            }
            // If our child has exited, note it — but don't fail yet. A concurrent start_session that
            // won the lifetime-lock race makes our loser child exit too, so keep polling for a short
            // grace so the winner can become reachable instead of returning a spurious error.
            if child_exit.is_none()
                && let Ok(Some(status)) = child.try_wait()
            {
                child_exit = Some(status);
                deadline = deadline.min(tokio::time::Instant::now() + CHILD_EXIT_GRACE);
            }
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
        description = "Read log lines after a cursor for incremental following. Returns the new \
        entries and a next_seq; pass next_seq as after_seq on the next call. If retention already \
        evicted unread lines, the response includes a gap object. Pass run_generation for a full \
        disk-backed retained run page; omit it for the bounded visible stream. Supports the same \
        raw/grep/min_level filters as get_logs; next_seq still advances past filtered-out lines."
    )]
    async fn follow_logs(&self, args: Parameters<FollowArgs>) -> Result<String, ErrorData> {
        let Parameters(args) = args;
        let grep = compile_grep(args.grep.as_deref())?;
        let min_level = parse_min_level(args.min_level.as_deref())?;
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
        // past filtered-out lines and following never re-fetches them.
        let next_seq = next_follow_cursor(&logs.lines, args.after_seq);
        let gap = follow_gap(&logs.lines, args.after_seq, args.run_generation);
        let entries = logproc::shape(
            &logs.lines,
            &logproc::Shape {
                raw: args.raw.unwrap_or(false),
                grep: grep.as_ref(),
                min_level,
                limit: None,
            },
        );
        let entries = serde_json::to_value(&entries).map_err(internal)?;
        ok_json(&json!({
            "service": args.service,
            "run_generation": args.run_generation,
            "entries": entries,
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
                        "run_generation": snapshot.run_generation,
                        "hint": "the run exited before becoming healthy — inspect get_logs for why",
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
                // Surface the facts we have rather than guessing "still building": the execution
                // sub-state and the latest healthcheck attempt distinguish "process up, probe
                // failing" from "still starting" without a heuristic.
                let latest = latest_health(&resolved.endpoint, &args.service)
                    .await
                    .map(|attempt| serde_json::to_value(bounded_attempt(attempt)))
                    .transpose()
                    .map_err(internal)?;
                return ok_json(&json!({
                    "status": "timeout",
                    "service": args.service,
                    "waited_secs": timeout.as_secs(),
                    "execution": serde_json::to_value(snapshot.execution).map_err(internal)?,
                    "run_generation": snapshot.run_generation,
                    "healthcheck_configured": snapshot.healthcheck_configured,
                    "health": serde_json::to_value(snapshot.health).map_err(internal)?,
                    "uptime_secs": snapshot.uptime.map(|uptime| uptime.as_secs()),
                    "latest_healthcheck": latest,
                    "hint": timeout_hint(&snapshot),
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

/// Best-effort connect + `Describe`; `None` if nothing is listening or it does not answer.
async fn describe(endpoint: &ControlEndpoint) -> Option<SessionInfo> {
    let mut client = Client::connect(endpoint).await.ok()?;
    client.describe().await.ok()
}

/// Report a session that already owns the endpoint, distinguishing a clean `Describe` from a live but
/// unanswerable peer (busy or a different version). `None` means nothing is listening, so a new
/// session can be spawned.
async fn already_running(endpoint: &ControlEndpoint) -> Option<Value> {
    if let Some(info) = describe(endpoint).await {
        return Some(json!({
            "started": false,
            "already_running": true,
            "session": info.name,
            "pid": info.pid,
            "config_path": info.config_path,
        }));
    }
    // Describe failed, but a peer that merely won't answer (busy / version mismatch) still holds the
    // ownership lock — don't spawn a doomed second session.
    if Client::connect(endpoint).await.is_ok() {
        return Some(json!({
            "started": false,
            "already_running": true,
            "note": "a session already owns this project but did not answer Describe \
                     (it may be busy or a different micromux version)",
        }));
    }
    None
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
