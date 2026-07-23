//! The authoritative session model.
//!
//! The scheduler owns lifecycle truth; this module materializes it as a queryable model that every
//! frontend (the TUI, the control socket, the MCP server) reads from. There is exactly one source
//! of truth: the scheduler writes lifecycle state through a possession-scoped
//! [`SessionModelWriter`] and gives each live run a scoped [`RunSink`] for its data records, while
//! adapters read through a cloneable [`SessionModelReader`]. The lack of a write method on the
//! reader is the security boundary — an adapter that only holds a reader can observe but never
//! mutate, and the only path by which it can affect state is the narrow
//! [`crate::ServiceControl`] command port, which the scheduler validates before writing anything.
//!
//! `Inner` is behind a synchronous [`parking_lot::RwLock`]; readers snapshot/clone under the lock,
//! drop it, and only then serialize — never holding the guard across an `.await`.

use std::collections::VecDeque;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use parking_lot::RwLock;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::health_check::Health;
use crate::scheduler::{LogUpdateKind, OutputStream, ServiceID};
use crate::service::RestartPolicy;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

const DEFAULT_LOG_RETAINED_RUNS: usize = 5;
const DEFAULT_MEMORY_LOG_MAX_LINES: usize = 1000;
const DEFAULT_MEMORY_LOG_MAX_BYTES: usize = 64 * MIB;

/// How many recent healthcheck attempts the model retains per service.
const HEALTH_HISTORY: usize = 8;
/// Per-attempt healthcheck output retention.
const HEALTH_OUTPUT_MAX_LINES: usize = 200;
/// Maximum retained lifecycle events per service.
pub const EVENT_HISTORY: usize = 256;
/// Capacity of the liveness-only change broadcast. A lagging subscriber loses only coalescible
/// notifications (it re-queries the model for content), never log bytes.
const CHANGE_CHANNEL_CAPACITY: usize = 1024;

/// The state a service has been *asked* to be in. `Disabled` is a desire, not an execution state —
/// a disabled service may still be draining.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Desired {
    /// The service should be running.
    Enabled,
    /// The service should be stopped and stay stopped.
    Disabled,
    /// A newer peer sent a desired state this binary does not know yet.
    #[serde(other)]
    Unknown,
}

/// The observed lifecycle phase of a service, independent of whether it is `Desired::Enabled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Execution {
    /// Not started yet (waiting on dependencies or the initial start), or idle while disabled.
    Pending,
    /// A process is being spawned.
    Starting,
    /// A process is live.
    Running,
    /// A process is live but draining after a restart/disable request.
    Stopping,
    /// The last process has exited (crash, completion, or stop).
    Exited,
    /// A newer peer sent an execution state this binary does not know yet.
    #[serde(other)]
    Unknown,
}

/// Origin category exposed on service snapshots.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum OriginKind {
    /// Loaded from the session configuration.
    #[default]
    Configured,
    /// Created through the control plane.
    Dynamic,
    /// A newer peer sent an origin this binary does not know yet.
    #[serde(other)]
    Unknown,
}

/// Why a dynamic service reached its terminal state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum RetiredReason {
    /// Explicitly stopped through the control plane.
    Stopped,
    /// Its lease expired.
    Expired,
    /// A newer peer sent a retirement reason this binary does not know yet.
    #[serde(other)]
    Unknown,
}

/// Public provenance and lifecycle metadata for a dynamic service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DynamicServiceInfo {
    /// Creation time in Unix milliseconds.
    pub created_at_unix_ms: u64,
    /// Lease expiry time in Unix milliseconds, or `None` for a session-lifetime lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix_ms: Option<u64>,
    /// Optional caller-supplied ownership label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Optimistic-concurrency revision.
    pub revision: u64,
    /// Terminal reason, present after retirement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired: Option<RetiredReason>,
    /// Retirement time in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at_unix_ms: Option<u64>,
}

#[derive(JsonSchema)]
#[expect(
    dead_code,
    reason = "schema-only shape used by #[schemars(with = \"Option<DurationSchema>\")]"
)]
struct DurationSchema {
    secs: u64,
    nanos: u32,
}

/// Effective healthcheck timing for a service run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HealthcheckConfig {
    /// Consecutive failed attempts required before the service becomes unhealthy.
    pub retries: usize,
    /// Delay between healthcheck attempts.
    #[schemars(with = "DurationSchema")]
    pub interval: Duration,
    /// Maximum duration of one healthcheck attempt.
    #[schemars(with = "DurationSchema")]
    pub timeout: Duration,
}

impl From<&crate::spec::HealthcheckSpec> for HealthcheckConfig {
    fn from(config: &crate::spec::HealthcheckSpec) -> Self {
        Self {
            retries: config.retries,
            interval: config.interval,
            timeout: config.timeout,
        }
    }
}

/// Active automatic-restart backoff for an exited service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RestartState {
    /// Delay selected for the next automatic restart attempt.
    #[schemars(with = "DurationSchema")]
    pub backoff_delay: Duration,
    /// Remaining automatic restarts for a bounded `on-failure` policy; `None` means unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarts_remaining: Option<usize>,
}

/// A point-in-time, serializable view of one service. This is the wire payload reused directly by
/// the control protocol (no DTO mirror).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ServiceSnapshot {
    /// Stable service identifier.
    pub id: ServiceID,
    /// Human-readable service name.
    pub name: String,
    /// Whether this service was configured or created at runtime.
    #[serde(default)]
    pub origin: OriginKind,
    /// Dynamic lifecycle metadata, present only for dynamic services.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic: Option<DynamicServiceInfo>,
    /// Requested state (`Disabled` is a desire, not an execution).
    pub desired: Desired,
    /// Observed lifecycle phase.
    pub execution: Execution,
    /// Latest resolved health for the currently owned live process. This is `None` whenever the
    /// service is not `Running`; latest-run probe attempts stay in `healthchecks()` until the next
    /// run begins and must not be presented as current health after exit.
    #[serde(default)]
    pub health: Option<Health>,
    /// Public name for the scheduler's `RunId`; bumps on every successful (re)start. `0` means the
    /// service has never started.
    #[serde(default)]
    pub run_generation: u64,
    /// Process ID of the direct child (the process-group leader). For wrapper commands such as
    /// `cargo run`, this identifies the wrapper rather than the eventual server. `None` unless a
    /// process is running, or when the pty backend did not report a pid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Wall-clock time when the current run started, in Unix milliseconds (the same unit as log
    /// entry timestamps and `since` filters). This identifies the run in operator-facing output;
    /// monotonic [`Self::uptime`] remains authoritative for elapsed time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_unix_ms: Option<u64>,
    /// Ports declared by the service config. These are informational hints, not a liveness signal.
    #[serde(default)]
    pub advertised_ports: Vec<u16>,
    /// Whether this service has a healthcheck configured.
    #[serde(default)]
    pub healthcheck_configured: bool,
    /// Effective healthcheck timing for the represented run, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthcheckConfig>,
    /// Whether the current command, working directory, ports, or effective healthcheck timing
    /// differs from the configuration captured for this run.
    #[serde(default)]
    pub config_stale: bool,
    /// Active automatic-restart backoff, if the scheduler is delaying the next attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_state: Option<RestartState>,
    /// Exit code of the most recently finished run, if any.
    #[serde(default)]
    pub last_exit_code: Option<i32>,
    /// The resolved program and arguments this service runs (argv), so a startup failure can be
    /// debugged without reopening the config.
    #[serde(default)]
    pub command: Vec<String>,
    /// The service's overridden working directory, if any; `None` means it inherits the session's
    /// working directory (the directory micromux was launched in).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Time since the current run started, refreshed at read time. `None` when not running.
    #[schemars(with = "Option<DurationSchema>")]
    #[serde(default)]
    pub uptime: Option<Duration>,
    /// The configured restart policy.
    #[serde(default)]
    pub restart_policy: RestartPolicy,
}

impl ServiceSnapshot {
    /// Construct the initial model snapshot for a configured service before the scheduler has
    /// attempted to start it.
    #[must_use]
    pub fn initial(
        id: ServiceID,
        name: String,
        advertised_ports: Vec<u16>,
        healthcheck: Option<HealthcheckConfig>,
        restart_policy: RestartPolicy,
        command: Vec<String>,
        working_dir: Option<String>,
    ) -> Self {
        Self {
            id,
            name,
            origin: OriginKind::Configured,
            dynamic: None,
            desired: Desired::Enabled,
            execution: Execution::Pending,
            health: None,
            run_generation: 0,
            pid: None,
            started_at_unix_ms: None,
            advertised_ports,
            healthcheck_configured: healthcheck.is_some(),
            healthcheck,
            config_stale: false,
            restart_state: None,
            last_exit_code: None,
            command,
            working_dir,
            uptime: None,
            restart_policy,
        }
    }
}

/// A line/byte retention limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLimit {
    /// Keep at most this many units.
    Bounded(usize),
    /// Do not bound this dimension.
    Unbounded,
}

impl LogLimit {
    /// Build a bounded limit, clamping zero to the minimum useful value.
    #[must_use]
    pub fn bounded(value: usize) -> Self {
        Self::Bounded(value.max(1))
    }

    fn as_option(self) -> Option<usize> {
        match self {
            Self::Bounded(value) => Some(value),
            Self::Unbounded => None,
        }
    }
}

/// Retention limits for the in-memory TUI/control tail. This is bounded by default so rendering and
/// snapshots remain cheap even when disk run logs are large.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryLogRetention {
    /// Maximum retained logical lines in memory.
    pub max_lines: LogLimit,
    /// Maximum retained logical bytes in memory.
    pub max_bytes: LogLimit,
}

impl Default for MemoryLogRetention {
    fn default() -> Self {
        Self {
            max_lines: LogLimit::Bounded(DEFAULT_MEMORY_LOG_MAX_LINES),
            max_bytes: LogLimit::Bounded(DEFAULT_MEMORY_LOG_MAX_BYTES),
        }
    }
}

/// Retention limits for disk-backed run logs. Individual run files are intentionally unbounded;
/// retention is by number of runs only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskLogRetention {
    /// Number of service runs retained, including the current/latest run.
    pub retained_runs: usize,
}

impl Default for DiskLogRetention {
    fn default() -> Self {
        Self {
            retained_runs: DEFAULT_LOG_RETAINED_RUNS,
        }
    }
}

/// Effective log retention for one normalized service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogRetention {
    /// In-memory tail used by the TUI/default log stream.
    pub memory: MemoryLogRetention,
    /// Disk-backed full run retention.
    pub disk: DiskLogRetention,
}

/// A single retained log line with a monotonic cursor.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LogLine {
    /// Monotonic sequence number; starts at 1 and survives eviction so a follower can resume
    /// without gaps or dupes. Cursor 0 means "before the first entry".
    pub seq: u64,
    /// The service run that produced this line.
    pub run_generation: u64,
    /// Wall-clock time when micromux ingested this record, in Unix milliseconds.
    #[serde(default)]
    pub timestamp_unix_ms: u64,
    /// The already-formatted line (stderr lines carry a `[stderr]` prefix, matching the TUI).
    pub line: String,
}

/// Summary of one retained service run's logs.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LogRunSummary {
    /// The scheduler run generation for this service run.
    pub run_generation: u64,
    /// Whether this is the latest known run for the service.
    pub current: bool,
    /// Disk-backed JSONL file for this run, when disk retention is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Number of retained lines for this run.
    pub line_count: usize,
    /// Sequence number of the first retained line, if any.
    pub first_seq: Option<u64>,
    /// Sequence number of the last retained line, if any.
    pub last_seq: Option<u64>,
}

/// Retained logs for one service run.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LogRun {
    /// The scheduler run generation for this service run.
    pub run_generation: u64,
    /// Whether this is the latest known run for the service.
    pub current: bool,
    /// Retained lines for the run.
    pub lines: Vec<LogLine>,
}

/// One healthcheck output line.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HealthLine {
    /// Which stream produced the output.
    pub stream: OutputStream,
    /// The output line.
    pub line: String,
}

/// The result of a finished healthcheck attempt.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
pub struct HealthResult {
    /// Whether the probe exited successfully.
    pub success: bool,
    /// Exit code of the probe process.
    pub exit_code: i32,
    /// Whether the probe was cancelled because the service run was stopping.
    #[serde(default)]
    pub cancelled: bool,
}

/// One healthcheck attempt and its (bounded) output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HealthAttempt {
    /// Run generation that produced this attempt.
    pub run_generation: u64,
    /// Monotonic attempt number within the current run.
    pub attempt: u64,
    /// The command that was executed for this probe.
    pub command: String,
    /// Captured probe output (bounded).
    pub output: Vec<HealthLine>,
    /// The final result, or `None` while the probe is still running.
    pub result: Option<HealthResult>,
}

/// One scheduler lifecycle fact retained for service diagnosis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ServiceEvent {
    /// Per-service monotonic sequence number.
    pub seq: u64,
    /// Event time in Unix milliseconds.
    pub at_unix_ms: u64,
    /// Run generation associated with the transition.
    pub run_generation: u64,
    /// Lifecycle transition category.
    pub kind: ServiceEventKind,
    /// One human-readable line describing the transition.
    pub detail: String,
    /// Exit code associated with an exit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Process id associated with a spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Backoff delay in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
    /// Dependencies preventing a start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_on: Option<Vec<ServiceID>>,
}

/// Scheduler lifecycle transition category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ServiceEventKind {
    /// A restart was requested.
    RestartRequested,
    /// An enable was requested.
    EnableRequested,
    /// A disable was requested.
    DisableRequested,
    /// Configured service definitions were reloaded.
    ConfigReloaded,
    /// A process could not be spawned.
    SpawnFailed,
    /// A process was spawned.
    Spawned,
    /// A running service became healthy.
    Healthy,
    /// A running service became unhealthy.
    Unhealthy,
    /// A process exited.
    Exited,
    /// An automatic-restart backoff was armed.
    BackoffScheduled,
    /// Dependencies are preventing a start.
    DependencyBlocked,
    /// Previously blocking dependencies became ready.
    DependencyReady,
    /// A dynamic service was created.
    Created,
    /// A dynamic service was replaced or revived.
    Replaced,
    /// A dynamic service was explicitly stopped or expired.
    Retired,
    /// A newer peer sent an event kind this binary does not know yet.
    #[serde(other)]
    Unknown,
}

/// What kind of change a [`SessionChange`] notification coalesces. The broadcast is liveness-only:
/// subscribers receive the kind and re-query the model for content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ChangeKind {
    /// The service's status snapshot changed.
    Status,
    /// New log content is available.
    Logs,
    /// Healthcheck history changed.
    Health,
    /// The service roster changed.
    Roster,
    /// Lifecycle timeline history changed.
    Events,
    /// A newer peer sent a change kind this binary does not know yet.
    #[serde(other)]
    Unknown,
}

/// A liveness-only change notification. `broadcast` drops for lagging receivers, so this must never
/// be the carrier of content — subscribers re-query the model.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionChange {
    /// The service that changed.
    pub service_id: ServiceID,
    /// What changed.
    pub kind: ChangeKind,
}

mod disk;
mod memory;
mod runlog;

use self::disk::{DiskLogOp, DiskLogRecord, DiskLogWorker, DiskLogWriter};
use self::memory::MemoryLogBuffer;
pub use self::memory::trim_to_last_bytes;
#[cfg(test)]
use self::runlog::RUN_LOG_OFFSET_CACHE;
use self::runlog::{
    RunLogEntry, RunLogReadIndex, create_spool_dir, read_run_log_file, unix_timestamp_ms,
};

struct ServiceEntry {
    snapshot: ServiceSnapshot,
    started_at: Option<Instant>,
    /// Only this run may update the visible logs and health history; older runs retain logs only.
    latest_begun_run: u64,
    visible: MemoryLogBuffer,
    runs: VecDeque<RunLogEntry>,
    next_log_seq: u64,
    log_retention: LogRetention,
    spool_dir: Option<PathBuf>,
    disk: Option<DiskLogWriter>,
    health: VecDeque<HealthAttempt>,
    events: VecDeque<ServiceEvent>,
    next_event_seq: u64,
}

struct RunLogSource {
    path: PathBuf,
    index: RunLogReadIndex,
}

impl ServiceEntry {
    fn new(
        snapshot: ServiceSnapshot,
        log_retention: LogRetention,
        spool_dir: Option<&Path>,
        disk: Option<DiskLogWriter>,
    ) -> Self {
        let latest_begun_run = snapshot.run_generation;
        let log_retention = LogRetention {
            disk: DiskLogRetention {
                retained_runs: log_retention.disk.retained_runs.max(1),
            },
            ..log_retention
        };
        Self {
            snapshot,
            started_at: None,
            latest_begun_run,
            visible: MemoryLogBuffer::new(log_retention.memory),
            runs: VecDeque::new(),
            next_log_seq: 1,
            log_retention,
            spool_dir: spool_dir.map(Path::to_path_buf),
            disk,
            health: VecDeque::new(),
            events: VecDeque::new(),
            next_event_seq: 1,
        }
    }

    /// Clone the snapshot, refreshing the volatile `uptime` from the stored start instant so a
    /// long-running service reports a live duration without the scheduler rewriting it on a timer.
    fn current_snapshot(&self) -> ServiceSnapshot {
        let mut snapshot = self.snapshot.clone();
        snapshot.uptime = self.started_at.map(|started| started.elapsed());
        snapshot
    }

    fn latest_run_generation(&self) -> u64 {
        self.snapshot.run_generation
    }

    fn ensure_run(&mut self, run_generation: u64) -> Option<&mut RunLogEntry> {
        if let Some(idx) = self
            .runs
            .iter()
            .position(|run| run.run_generation == run_generation)
        {
            return self.runs.get_mut(idx);
        }

        self.runs.push_back(RunLogEntry::new(
            &self.snapshot.id,
            run_generation,
            self.spool_dir.as_deref(),
            self.disk.as_ref(),
        ));
        while self.runs.len() > self.log_retention.disk.retained_runs {
            if let Some(mut evicted) = self.runs.pop_front() {
                evicted.enqueue_remove(self.disk.as_ref());
            }
        }
        self.runs.back_mut()
    }

    fn begin_run(&mut self, run_generation: u64) {
        self.latest_begun_run = run_generation;
        if let Some(run) = self.ensure_run(run_generation) {
            run.live_snapshot_id = None;
        }
        self.health.clear();
    }

    fn latest_current_health(&self) -> Option<HealthAttempt> {
        if self.snapshot.execution == Execution::Running {
            return self
                .health
                .iter()
                .rev()
                .find(|attempt| attempt.run_generation == self.snapshot.run_generation)
                .cloned();
        }
        None
    }

    fn append_log(&mut self, run_generation: u64, update: LogUpdateKind, line: String) {
        // A late record from a run the retention ring already evicted must not resurrect it:
        // `ensure_run` would re-add the old generation *behind* newer runs and the ring would
        // evict a newer run in its place. Late records are only kept while their run's entry
        // still exists.
        if run_generation != self.latest_begun_run
            && !self
                .runs
                .iter()
                .any(|run| run.run_generation == run_generation)
        {
            return;
        }
        let mut next_seq = self.next_log_seq;
        let disk = self.disk.clone();
        let timestamp_unix_ms = unix_timestamp_ms();
        let Some((op, line)) = ({
            let Some(run) = self.ensure_run(run_generation) else {
                return;
            };
            let (op, seq) = match update {
                LogUpdateKind::Append => {
                    run.live_snapshot_id = None;
                    let seq = next_seq;
                    next_seq = next_seq.saturating_add(1);
                    run.append_metadata(seq);
                    (DiskLogOp::Append, seq)
                }
                LogUpdateKind::LiveSnapshot { id } => {
                    let resolved = if run.live_snapshot_id == Some(id) {
                        run.last_seq.map(|seq| {
                            run.replace_metadata(seq);
                            (DiskLogOp::ReplaceLast, seq)
                        })
                    } else {
                        None
                    };
                    let (op, seq) = resolved.unwrap_or_else(|| {
                        let seq = next_seq;
                        next_seq = next_seq.saturating_add(1);
                        run.append_metadata(seq);
                        (DiskLogOp::Append, seq)
                    });
                    run.live_snapshot_id = Some(id);
                    (op, seq)
                }
            };

            let line = LogLine {
                seq,
                run_generation,
                timestamp_unix_ms,
                line,
            };
            let record = DiskLogRecord {
                seq,
                run_generation,
                timestamp_unix_ms,
                op,
                line: line.line.clone(),
            };
            run.enqueue_write(disk.as_ref(), record);
            Some((op, line))
        }) else {
            return;
        };
        self.next_log_seq = next_seq;

        if run_generation == self.latest_begun_run {
            match op {
                DiskLogOp::Append => self.visible.append(line),
                DiskLogOp::ReplaceLast => self.visible.replace_last(line),
            }
        }
    }

    fn clear_visible_logs(&mut self) {
        self.visible.clear_before_next_seq(self.next_log_seq);
        if let Some(run) = self.runs.back_mut() {
            run.live_snapshot_id = None;
        }
    }

    fn log_runs(&self) -> Vec<LogRunSummary> {
        let latest = self.latest_run_generation();
        let mut runs = self
            .runs
            .iter()
            .filter(|run| run.path.is_some())
            .map(|run| run.summary(run.run_generation == latest))
            .collect::<Vec<_>>();
        if latest != 0 && !self.runs.iter().any(|run| run.run_generation == latest) {
            runs.push(LogRunSummary {
                run_generation: latest,
                current: true,
                path: None,
                line_count: 0,
                first_seq: None,
                last_seq: None,
            });
        }
        runs
    }

    fn run_log_source(&self, run_generation: u64) -> Option<RunLogSource> {
        self.runs
            .iter()
            .find(|run| run.run_generation == run_generation)
            .and_then(|run| {
                run.path.clone().map(|path| RunLogSource {
                    path,
                    index: run.read_index.clone(),
                })
            })
    }

    fn synthetic_empty_run(&self, run_generation: u64) -> Option<LogRun> {
        let latest = self.latest_run_generation();
        if run_generation == 0
            || run_generation != latest
            || self
                .runs
                .iter()
                .any(|run| run.run_generation == run_generation)
        {
            return None;
        }
        Some(LogRun {
            run_generation,
            current: true,
            lines: Vec::new(),
        })
    }

    fn install_run_log_index(
        &mut self,
        run_generation: u64,
        path: &Path,
        index: RunLogReadIndex,
    ) -> Option<bool> {
        let latest = self.latest_run_generation();
        let run = self
            .runs
            .iter_mut()
            .find(|run| run.run_generation == run_generation)?;
        if run.path.as_deref() != Some(path) {
            return None;
        }
        run.read_index = index;
        Some(run.run_generation == latest)
    }

    fn visible_lines(&self, tail: Option<usize>) -> Vec<LogLine> {
        self.visible.lines(tail)
    }

    fn visible_lines_since(&self, after: u64) -> (Option<u64>, Vec<LogLine>) {
        self.visible.lines_since(after)
    }

    fn reconfigure_log_retention(&mut self, log_retention: LogRetention) {
        let log_retention = LogRetention {
            disk: DiskLogRetention {
                retained_runs: log_retention.disk.retained_runs.max(1),
            },
            ..log_retention
        };
        self.visible.reconfigure(log_retention.memory);
        self.log_retention = log_retention;
        while self.runs.len() > self.log_retention.disk.retained_runs {
            if let Some(mut evicted) = self.runs.pop_front() {
                evicted.enqueue_remove(self.disk.as_ref());
            }
        }
    }

    fn remove_retained_run_files(&mut self) {
        for run in &mut self.runs {
            run.enqueue_remove(self.disk.as_ref());
        }
    }
}

struct Inner {
    services: RwLock<IndexMap<ServiceID, ServiceEntry>>,
    change_tx: broadcast::Sender<SessionChange>,
    spool_dir: Option<PathBuf>,
    _spool_lock: Option<File>,
    disk: Option<DiskLogWorker>,
    disk_writer: Option<DiskLogWriter>,
}

impl Inner {
    fn publish(&self, service_id: &ServiceID, kind: ChangeKind) {
        // A send error only means there are no subscribers right now; that is fine.
        let _ = self.change_tx.send(SessionChange {
            service_id: service_id.clone(),
            kind,
        });
    }

    fn flush_disk(&self) {
        if let Some(disk) = &self.disk {
            disk.flush();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.services.get_mut().clear();
        self.disk_writer.take();
        let disk_stopped = if let Some(disk) = self.disk.as_mut() {
            disk.shutdown()
        } else {
            true
        };
        if disk_stopped
            && let Some(path) = self.spool_dir.take()
            && let Err(err) = fs::remove_dir_all(&path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(?err, path = %path.display(), "failed to remove session log spool");
        }
    }
}

/// Read capability over the session model. `Clone`, handed to every adapter (TUI, control server,
/// MCP). It has no write methods — that absence is the boundary.
#[derive(Clone)]
pub struct SessionModelReader {
    inner: Arc<Inner>,
}

impl SessionModelReader {
    fn read_run_log(
        &self,
        id: &str,
        run_generation: u64,
        tail: Option<usize>,
        after: Option<u64>,
        limit: Option<usize>,
    ) -> Option<LogRun> {
        enum Target {
            Disk(RunLogSource),
            Synthetic(LogRun),
        }

        let target = {
            let guard = self.inner.services.read();
            let entry = guard.get(id)?;
            if let Some(source) = entry.run_log_source(run_generation) {
                Target::Disk(source)
            } else {
                Target::Synthetic(entry.synthetic_empty_run(run_generation)?)
            }
        };

        match target {
            Target::Synthetic(run) => Some(run),
            Target::Disk(source) => {
                self.inner.flush_disk();
                let (lines, index) =
                    read_run_log_file(&source.path, tail, after, limit, source.index)?;
                let current = {
                    let mut guard = self.inner.services.write();
                    let entry = guard.get_mut(id)?;
                    entry.install_run_log_index(run_generation, &source.path, index)?
                };
                Some(LogRun {
                    run_generation,
                    current,
                    lines,
                })
            }
        }
    }

    /// A snapshot of every service, in configuration order, with `uptime` refreshed to now.
    #[must_use]
    pub fn services(&self) -> Vec<ServiceSnapshot> {
        let guard = self.inner.services.read();
        guard.values().map(ServiceEntry::current_snapshot).collect()
    }

    /// The snapshot of a single service, if present.
    #[must_use]
    pub fn service(&self, id: &str) -> Option<ServiceSnapshot> {
        let guard = self.inner.services.read();
        guard.get(id).map(ServiceEntry::current_snapshot)
    }

    /// The retained log lines for a service. `tail` bounds the result to the most recent lines.
    #[must_use]
    pub fn logs(&self, id: &str, tail: Option<usize>) -> Vec<LogLine> {
        let guard = self.inner.services.read();
        guard
            .get(id)
            .map(|entry| entry.visible_lines(tail))
            .unwrap_or_default()
    }

    /// Visible log records with `seq > after`, plus the first retained sequence so incremental
    /// consumers can prune entries the visible buffer has evicted or cleared.
    #[must_use]
    pub fn logs_since(&self, id: &str, after: u64) -> (Option<u64>, Vec<LogLine>) {
        let guard = self.inner.services.read();
        guard
            .get(id)
            .map_or((None, Vec::new()), |entry| entry.visible_lines_since(after))
    }

    /// Summaries of retained log runs for a service, oldest retained run first.
    #[must_use]
    pub fn log_runs(&self, id: &str) -> Vec<LogRunSummary> {
        let guard = self.inner.services.read();
        guard
            .get(id)
            .map(ServiceEntry::log_runs)
            .unwrap_or_default()
    }

    /// The retained logs for one run of a service.
    #[must_use]
    pub fn run_log(&self, id: &str, run_generation: u64, tail: Option<usize>) -> Option<LogRun> {
        self.read_run_log(id, run_generation, tail, None, None)
    }

    /// The retained logs for one run, strictly after a monotonic cursor.
    #[must_use]
    pub fn run_log_after(
        &self,
        id: &str,
        run_generation: u64,
        after: Option<u64>,
        limit: Option<usize>,
    ) -> Option<LogRun> {
        self.read_run_log(id, run_generation, None, after, limit)
    }

    /// Retained healthcheck history for a service's current/latest run (oldest first).
    ///
    /// This remains queryable after exit for diagnosis, but is cleared when the next run begins.
    #[must_use]
    pub fn healthchecks(&self, id: &str) -> Vec<HealthAttempt> {
        let guard = self.inner.services.read();
        guard
            .get(id)
            .map(|entry| entry.health.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The most recent healthcheck attempt for a service's current live run, if any.
    ///
    /// Health is scoped to the run micromux currently owns. After exit, a probe might still succeed
    /// against another process or an always-true command, so latest-run attempts remain queryable
    /// through [`Self::healthchecks`] for diagnosis but are not current health.
    #[must_use]
    pub fn latest_health(&self, id: &str) -> Option<HealthAttempt> {
        let guard = self.inner.services.read();
        guard.get(id).and_then(ServiceEntry::latest_current_health)
    }

    /// Retained lifecycle events, either forward from `after` or as the newest chronological tail.
    ///
    /// The returned boolean is true when retention or `tail` omitted matching events.
    #[must_use]
    pub fn events(
        &self,
        id: &str,
        after: Option<u64>,
        tail: Option<usize>,
    ) -> (Vec<ServiceEvent>, bool) {
        let guard = self.inner.services.read();
        let Some(entry) = guard.get(id) else {
            return (Vec::new(), false);
        };
        let limit = tail.unwrap_or(EVENT_HISTORY).min(EVENT_HISTORY);
        let retention_truncated = entry
            .events
            .front()
            .is_some_and(|first| first.seq > after.unwrap_or(0).saturating_add(1));
        let matching = entry
            .events
            .iter()
            .filter(|event| after.is_none_or(|after| event.seq > after))
            .cloned()
            .collect::<Vec<_>>();
        let truncated = retention_truncated || matching.len() > limit;
        if after.is_some() {
            (matching.into_iter().take(limit).collect(), truncated)
        } else {
            let skip = matching.len().saturating_sub(limit);
            (matching.into_iter().skip(skip).collect(), truncated)
        }
    }

    /// Subscribe to liveness-only change notifications. Re-query the model for content on each.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<SessionChange> {
        self.inner.change_tx.subscribe()
    }
}

/// Write capability over the session model — capability-by-possession, *not* a restricted-visibility
/// path. (`pub(in crate::scheduler)` would not compile: a restricted path must name an *ancestor*
/// module, and `crate::scheduler` is a sibling of `crate::model`.) Instead this type is
/// `pub(crate)`, has no public constructor, and is `!Clone`; the only way to obtain one is
/// [`new`], which hands it straight into the scheduler future. It never appears in
/// `Handles`, so no adapter can hold one.
pub(crate) struct SessionModelWriter {
    inner: Arc<Inner>,
}

/// Write capability for one service run's logs and healthcheck records.
///
/// The scheduler mints this capability for the run's PTY reader and healthcheck tasks. It never
/// reaches adapters, and its embedded run identity lets the model keep late records in the
/// retained run without exposing them as current data.
#[derive(Clone)]
pub(crate) struct RunSink {
    inner: Arc<Inner>,
    service_id: ServiceID,
    run_generation: u64,
}

impl RunSink {
    pub(crate) fn append_log(&self, stream: OutputStream, update: LogUpdateKind, line: String) {
        let line = match stream {
            OutputStream::Stdout | OutputStream::Unknown => line,
            OutputStream::Stderr => format!("[stderr] {line}"),
        };
        {
            let mut guard = self.inner.services.write();
            let Some(entry) = guard.get_mut(&self.service_id) else {
                return;
            };
            entry.append_log(self.run_generation, update, line);
        }
        self.inner.publish(&self.service_id, ChangeKind::Logs);
    }

    pub(crate) fn start_health_attempt(&self, attempt: u64, command: String) {
        {
            let mut guard = self.inner.services.write();
            let Some(entry) = guard.get_mut(&self.service_id) else {
                return;
            };
            if entry.latest_begun_run != self.run_generation {
                return;
            }
            while entry.health.len() >= HEALTH_HISTORY {
                entry.health.pop_front();
            }
            entry.health.push_back(HealthAttempt {
                run_generation: self.run_generation,
                attempt,
                command,
                output: Vec::new(),
                result: None,
            });
        }
        self.inner.publish(&self.service_id, ChangeKind::Health);
    }

    pub(crate) fn append_health_line(&self, attempt: u64, stream: OutputStream, line: String) {
        {
            let mut guard = self.inner.services.write();
            let Some(entry) = guard.get_mut(&self.service_id) else {
                return;
            };
            if entry.latest_begun_run != self.run_generation {
                return;
            }
            let Some(attempt_entry) = entry.health.iter_mut().find(|entry| {
                entry.run_generation == self.run_generation && entry.attempt == attempt
            }) else {
                return;
            };
            while attempt_entry.output.len() >= HEALTH_OUTPUT_MAX_LINES {
                attempt_entry.output.remove(0);
            }
            attempt_entry.output.push(HealthLine { stream, line });
        }
        self.inner.publish(&self.service_id, ChangeKind::Health);
    }

    pub(crate) fn finish_health_attempt(
        &self,
        attempt: u64,
        success: bool,
        exit_code: i32,
        cancelled: bool,
    ) {
        {
            let mut guard = self.inner.services.write();
            let Some(entry) = guard.get_mut(&self.service_id) else {
                return;
            };
            if entry.latest_begun_run != self.run_generation {
                return;
            }
            let Some(attempt_entry) = entry.health.iter_mut().find(|entry| {
                entry.run_generation == self.run_generation && entry.attempt == attempt
            }) else {
                return;
            };
            attempt_entry.result = Some(HealthResult {
                success,
                exit_code,
                cancelled,
            });
        }
        self.inner.publish(&self.service_id, ChangeKind::Health);
    }
}

impl SessionModelWriter {
    /// Add a service to the end of the roster.
    pub(crate) fn insert_service(&self, snapshot: ServiceSnapshot, retention: LogRetention) {
        let service_id = snapshot.id.clone();
        {
            let mut guard = self.inner.services.write();
            if guard.contains_key(&service_id) {
                tracing::warn!(service_id, "ignoring duplicate service insertion");
                return;
            }
            guard.insert(
                service_id.clone(),
                ServiceEntry::new(
                    snapshot,
                    retention,
                    self.inner.spool_dir.as_deref(),
                    self.inner.disk_writer.clone(),
                ),
            );
        }
        self.inner.publish(&service_id, ChangeKind::Roster);
    }

    /// Remove a fully drained service from the roster.
    pub(crate) fn remove_service(&self, id: &ServiceID) {
        let removed = self
            .inner
            .services
            .write()
            .shift_remove(id)
            .map(|mut entry| entry.remove_retained_run_files())
            .is_some();
        if removed {
            self.inner.publish(id, ChangeKind::Roster);
        } else {
            tracing::warn!(service_id = id, "ignoring removal of unknown service");
        }
    }

    /// Append one lifecycle event, assigning its per-service sequence number.
    pub(crate) fn append_event(&self, id: &ServiceID, mut event: ServiceEvent) {
        {
            let mut guard = self.inner.services.write();
            let Some(entry) = guard.get_mut(id) else {
                tracing::warn!(service_id = id, "ignoring event for unknown service");
                return;
            };
            event.seq = entry.next_event_seq;
            entry.next_event_seq = entry.next_event_seq.saturating_add(1);
            while entry.events.len() >= EVENT_HISTORY {
                entry.events.pop_front();
            }
            entry.events.push_back(event);
        }
        self.inner.publish(id, ChangeKind::Events);
    }

    pub(crate) fn run_sink(&self, service_id: &ServiceID, run_generation: u64) -> RunSink {
        RunSink {
            inner: self.inner.clone(),
            service_id: service_id.clone(),
            run_generation,
        }
    }

    /// Publish a status snapshot for a service. `started_at` is `Some` while a run is live so the
    /// reader can refresh `uptime`.
    pub(crate) fn write_snapshot(&self, snapshot: ServiceSnapshot, started_at: Option<Instant>) {
        let service_id = snapshot.id.clone();
        {
            let mut guard = self.inner.services.write();
            if let Some(entry) = guard.get_mut(&service_id) {
                entry.snapshot = snapshot;
                entry.started_at = started_at;
            } else {
                tracing::warn!(service_id, "ignoring snapshot for unknown service");
                return;
            }
        }
        self.inner.publish(&service_id, ChangeKind::Status);
    }

    /// Append a log line using the scheduler's logical line-update semantics.
    #[cfg(test)]
    pub(crate) fn append_log(
        &self,
        id: &ServiceID,
        run_generation: u64,
        stream: OutputStream,
        update: LogUpdateKind,
        line: String,
    ) {
        self.run_sink(id, run_generation)
            .append_log(stream, update, line);
    }

    /// Hide older retained logs from the default visible log stream (e.g. on manual restart).
    pub(crate) fn clear_logs(&self, id: &ServiceID) {
        {
            let mut guard = self.inner.services.write();
            let Some(entry) = guard.get_mut(id) else {
                return;
            };
            entry.clear_visible_logs();
        }
        self.inner.publish(id, ChangeKind::Logs);
    }

    /// Begin a started run's retained state.
    pub(crate) fn begin_run(&self, id: &ServiceID, run_generation: u64) {
        {
            let mut guard = self.inner.services.write();
            if let Some(entry) = guard.get_mut(id) {
                entry.begin_run(run_generation);
            }
        }
        self.inner.publish(id, ChangeKind::Logs);
        self.inner.publish(id, ChangeKind::Health);
    }

    /// Apply updated log retention from a reloaded service definition.
    pub(crate) fn reconfigure_log_retention(&self, id: &ServiceID, log_retention: LogRetention) {
        {
            let mut guard = self.inner.services.write();
            let Some(entry) = guard.get_mut(id) else {
                return;
            };
            entry.reconfigure_log_retention(log_retention);
        }
        self.inner.publish(id, ChangeKind::Logs);
    }

    /// Begin a new healthcheck attempt, evicting the oldest beyond the retained history.
    #[cfg(test)]
    pub(crate) fn start_health_attempt(
        &self,
        id: &ServiceID,
        run_generation: u64,
        attempt: u64,
        command: String,
    ) {
        self.run_sink(id, run_generation)
            .start_health_attempt(attempt, command);
    }

    /// Append output to an in-progress healthcheck attempt.
    #[cfg(test)]
    pub(crate) fn append_health_line(
        &self,
        id: &ServiceID,
        run_generation: u64,
        attempt: u64,
        stream: OutputStream,
        line: String,
    ) {
        self.run_sink(id, run_generation)
            .append_health_line(attempt, stream, line);
    }

    /// Record the result of a finished healthcheck attempt.
    #[cfg(test)]
    pub(crate) fn finish_health_attempt(
        &self,
        id: &ServiceID,
        run_generation: u64,
        attempt: u64,
        success: bool,
        exit_code: i32,
        cancelled: bool,
    ) {
        self.run_sink(id, run_generation)
            .finish_health_attempt(attempt, success, exit_code, cancelled);
    }
}

/// Build the model with explicit per-service log retention. This is the only way to mint a
/// [`SessionModelWriter`].
pub(crate) fn new_with_retention(
    initial: impl IntoIterator<Item = (ServiceSnapshot, LogRetention)>,
    spool_dir: Option<PathBuf>,
    spool_lock: Option<File>,
) -> (SessionModelReader, SessionModelWriter) {
    let (disk, disk_writer) = if spool_dir.is_some() {
        let (worker, writer) = DiskLogWorker::spawn();
        (Some(worker), Some(writer))
    } else {
        (None, None)
    };
    let mut services = IndexMap::new();
    for (snapshot, log_retention) in initial {
        services.insert(
            snapshot.id.clone(),
            ServiceEntry::new(
                snapshot,
                log_retention,
                spool_dir.as_deref(),
                disk_writer.clone(),
            ),
        );
    }
    let (change_tx, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
    let inner = Arc::new(Inner {
        services: RwLock::new(services),
        change_tx,
        spool_dir,
        _spool_lock: spool_lock,
        disk,
        disk_writer,
    });
    (
        SessionModelReader {
            inner: Arc::clone(&inner),
        },
        SessionModelWriter { inner },
    )
}

/// Build the model using a private, session-owned spool directory for disk run logs.
pub(crate) fn new(
    initial: impl IntoIterator<Item = (ServiceSnapshot, LogRetention)>,
) -> (SessionModelReader, SessionModelWriter) {
    let (spool_dir, spool_lock) = create_spool_dir().unzip();
    new_with_retention(initial, spool_dir, spool_lock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{initial_model_entry as entry, initial_snapshot as snapshot};
    use serde_json::json;
    use similar_asserts::assert_eq;

    fn running_snapshot(id: &str) -> ServiceSnapshot {
        let mut snapshot = snapshot(id);
        snapshot.execution = Execution::Running;
        snapshot.healthcheck_configured = true;
        snapshot.run_generation = 1;
        snapshot
    }

    #[test]
    fn wire_enums_accept_unknown_unit_variants() {
        assert_eq!(
            serde_json::from_str::<Desired>("\"Paused\"").unwrap(),
            Desired::Unknown
        );
        assert_eq!(
            serde_json::from_str::<Execution>("\"Crashed\"").unwrap(),
            Execution::Unknown
        );
        assert_eq!(
            serde_json::from_str::<Health>("\"Starting\"").unwrap(),
            Health::Unknown
        );
        assert_eq!(
            serde_json::from_str::<ChangeKind>("\"Metrics\"").unwrap(),
            ChangeKind::Unknown
        );
        assert_eq!(
            serde_json::from_str::<OutputStream>("\"Combined\"").unwrap(),
            OutputStream::Unknown
        );
        assert_eq!(
            serde_json::from_str::<OriginKind>("\"Templated\"").unwrap(),
            OriginKind::Unknown
        );
        assert_eq!(
            serde_json::from_str::<RetiredReason>("\"Preempted\"").unwrap(),
            RetiredReason::Unknown
        );
        assert_eq!(
            serde_json::from_str::<ServiceEventKind>("\"Migrated\"").unwrap(),
            ServiceEventKind::Unknown
        );
    }

    #[test]
    fn service_snapshot_accepts_missing_additive_fields() -> serde_json::Result<()> {
        let snapshot = serde_json::from_value::<ServiceSnapshot>(json!({
            "id": "svc",
            "name": "svc",
            "desired": "Enabled",
            "execution": "Running"
        }))?;

        assert_eq!(snapshot.run_generation, 0);
        assert_eq!(snapshot.pid, None);
        assert_eq!(snapshot.started_at_unix_ms, None);
        assert_eq!(snapshot.advertised_ports, Vec::<u16>::new());
        assert!(!snapshot.healthcheck_configured);
        assert_eq!(snapshot.healthcheck, None);
        assert!(!snapshot.config_stale);
        assert_eq!(snapshot.restart_state, None);
        assert_eq!(snapshot.restart_policy, RestartPolicy::Never);
        assert_eq!(snapshot.command, Vec::<String>::new());
        assert_eq!(snapshot.health, None);
        assert_eq!(snapshot.origin, OriginKind::Configured);
        assert!(snapshot.dynamic.is_none());

        let encoded = serde_json::to_string(&snapshot)?;
        let decoded = serde_json::from_str::<ServiceSnapshot>(&encoded)?;
        assert_eq!(decoded.id, "svc");
        assert_eq!(decoded.pid, None);
        assert_eq!(decoded.started_at_unix_ms, None);
        Ok(())
    }

    #[test]
    fn service_snapshot_serializes_advertised_ports() -> serde_json::Result<()> {
        let mut snapshot = snapshot("svc");
        snapshot.advertised_ports = vec![3100];

        let value = serde_json::to_value(snapshot)?;
        assert_eq!(value.get("advertised_ports"), Some(&json!([3100])));
        assert_eq!(value.get("open_ports"), None);
        Ok(())
    }

    #[test]
    fn log_line_accepts_missing_ingest_timestamp() {
        let line = serde_json::from_value::<LogLine>(json!({
            "seq": 7,
            "run_generation": 1,
            "line": "old payload"
        }))
        .unwrap();

        assert_eq!(line.timestamp_unix_ms, 0);
        assert_eq!(line.line, "old payload");
    }

    #[test]
    fn health_result_accepts_missing_cancelled_field() {
        let result = serde_json::from_value::<HealthResult>(json!({
            "success": false,
            "exit_code": -1
        }))
        .unwrap();

        assert!(!result.cancelled);
    }

    #[test]
    fn service_snapshot_uptime_schema_matches_duration_wire_shape() -> color_eyre::Result<()> {
        let mut snapshot = snapshot("svc");
        snapshot.uptime = Some(Duration::new(3, 4));

        let value = serde_json::to_value(&snapshot)?;
        assert!(value.pointer("/uptime/secs").is_some());
        assert!(value.pointer("/uptime/nanos").is_some());

        let schema = serde_json::to_string(&schemars::schema_for!(ServiceSnapshot))?;
        assert!(schema.contains("\"secs\""));
        assert!(schema.contains("\"nanos\""));
        Ok(())
    }

    fn unique_spool_dir(prefix: &str) -> PathBuf {
        let prefix = format!("micromux-{prefix}-");
        tempfile::Builder::new()
            .prefix(&prefix)
            .tempdir()
            .expect("create temp spool")
            .keep()
    }

    #[test]
    fn append_assigns_monotonic_sequence_numbers() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();
        writer.begin_run(&id, 1);
        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "a".into(),
        );
        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "b".into(),
        );

        let lines = reader.logs(&id, None);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines.first().map(|l| l.seq), Some(1));
        assert_eq!(lines.get(1).map(|l| l.seq), Some(2));
        assert_eq!(lines.get(1).map(|l| l.run_generation), Some(1));
        assert_eq!(lines.get(1).map(|l| l.line.clone()), Some("b".to_string()));
    }

    #[test]
    fn stderr_lines_are_prefixed_like_the_model() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();
        writer.begin_run(&id, 1);
        writer.append_log(
            &id,
            1,
            OutputStream::Stderr,
            LogUpdateKind::Append,
            "boom".into(),
        );
        let lines = reader.logs(&id, None);
        assert_eq!(
            lines.first().map(|l| l.line.clone()),
            Some("[stderr] boom".to_string())
        );
    }

    #[test]
    fn reconfigured_memory_retention_trims_existing_visible_logs() {
        let retention = LogRetention {
            memory: MemoryLogRetention {
                max_lines: LogLimit::Bounded(10),
                max_bytes: LogLimit::Unbounded,
            },
            disk: DiskLogRetention::default(),
        };
        let (reader, writer) = new([(snapshot("svc"), retention)]);
        let id = "svc".to_string();
        writer.begin_run(&id, 1);
        for line in ["a", "b", "c"] {
            writer.append_log(
                &id,
                1,
                OutputStream::Stdout,
                LogUpdateKind::Append,
                line.to_string(),
            );
        }

        writer.reconfigure_log_retention(
            &id,
            LogRetention {
                memory: MemoryLogRetention {
                    max_lines: LogLimit::Bounded(1),
                    max_bytes: LogLimit::Unbounded,
                },
                disk: DiskLogRetention::default(),
            },
        );

        let lines = reader
            .logs(&id, None)
            .into_iter()
            .map(|line| line.line)
            .collect::<Vec<_>>();
        assert_eq!(lines, vec!["c"]);
    }

    #[test]
    fn live_snapshot_replaces_same_id_and_appends_new_id() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();
        writer.begin_run(&id, 1);
        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 7 },
            "frame one".into(),
        );
        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 7 },
            "frame two".into(),
        );
        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 8 },
            "frame three".into(),
        );
        let lines: Vec<String> = reader.logs(&id, None).into_iter().map(|l| l.line).collect();
        assert_eq!(
            lines,
            vec!["frame two".to_string(), "frame three".to_string()]
        );
    }

    #[test]
    fn logs_since_returns_incremental_lines_and_retention_marker() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();
        writer.begin_run(&id, 1);
        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "a".into(),
        );
        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "b".into(),
        );

        let (first, lines) = reader.logs_since(&id, 0);
        assert_eq!(first, Some(1));
        assert_eq!(
            lines
                .iter()
                .map(|line| line.line.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        let (first, lines) = reader.logs_since(&id, 1);
        assert_eq!(first, Some(1));
        assert_eq!(
            lines
                .iter()
                .map(|line| line.line.as_str())
                .collect::<Vec<_>>(),
            vec!["b"]
        );

        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 7 },
            "frame one".into(),
        );
        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 7 },
            "frame two".into(),
        );
        let (first, lines) = reader.logs_since(&id, 2);
        assert_eq!(first, Some(1));
        assert_eq!(
            lines
                .iter()
                .map(|line| (line.seq, line.line.as_str()))
                .collect::<Vec<_>>(),
            vec![(3, "frame two")]
        );
        assert!(reader.logs_since(&id, 3).1.is_empty());

        writer.reconfigure_log_retention(
            &id,
            LogRetention {
                memory: MemoryLogRetention {
                    max_lines: LogLimit::Bounded(1),
                    max_bytes: LogLimit::Unbounded,
                },
                disk: DiskLogRetention::default(),
            },
        );
        let (first, lines) = reader.logs_since(&id, 0);
        assert_eq!(first, Some(3));
        assert_eq!(
            lines
                .iter()
                .map(|line| line.line.as_str())
                .collect::<Vec<_>>(),
            vec!["frame two"]
        );

        writer.clear_logs(&id);
        let (first, lines) = reader.logs_since(&id, 0);
        assert_eq!(first, None);
        assert!(lines.is_empty());
    }

    #[test]
    fn tail_limits_returned_lines() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();
        writer.begin_run(&id, 1);
        for i in 0..5 {
            writer.append_log(
                &id,
                1,
                OutputStream::Stdout,
                LogUpdateKind::Append,
                format!("line {i}"),
            );
        }
        let lines: Vec<String> = reader
            .logs(&id, Some(2))
            .into_iter()
            .map(|l| l.line)
            .collect();
        assert_eq!(lines, vec!["line 3".to_string(), "line 4".to_string()]);
    }

    #[test]
    fn visible_logs_span_runs_until_cleared_but_runs_remain_queryable() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();

        writer.begin_run(&id, 1);
        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "run one".into(),
        );
        writer.begin_run(&id, 2);
        writer.append_log(
            &id,
            2,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "run two".into(),
        );

        let visible: Vec<String> = reader.logs(&id, None).into_iter().map(|l| l.line).collect();
        assert_eq!(visible, vec!["run one".to_string(), "run two".to_string()]);

        writer.clear_logs(&id);
        writer.append_log(
            &id,
            2,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "after clear".into(),
        );

        let visible: Vec<String> = reader.logs(&id, None).into_iter().map(|l| l.line).collect();
        assert_eq!(visible, vec!["after clear".to_string()]);
        let run_one: Vec<String> = reader
            .run_log(&id, 1, None)
            .expect("run one retained")
            .lines
            .into_iter()
            .map(|l| l.line)
            .collect();
        assert_eq!(run_one, vec!["run one".to_string()]);
    }

    #[test]
    fn late_log_is_retained_with_its_run_but_not_visible() -> color_eyre::Result<()> {
        let (reader, writer) =
            new_with_retention([entry("svc")], Some(unique_spool_dir("late-run-log")), None);
        let id = "svc".to_string();

        writer.begin_run(&id, 1);
        let first_run = writer.run_sink(&id, 1);
        first_run.append_log(
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "before restart".to_string(),
        );
        writer.begin_run(&id, 2);
        writer.append_log(
            &id,
            2,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "current run".to_string(),
        );

        first_run.append_log(
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "late first run".to_string(),
        );

        let visible = reader
            .logs(&id, None)
            .into_iter()
            .map(|line| line.line)
            .collect::<Vec<_>>();
        assert_eq!(visible, vec!["before restart", "current run"]);
        let retained = reader
            .run_log(&id, 1, None)
            .ok_or_else(|| color_eyre::eyre::eyre!("missing retained first run"))?
            .lines
            .into_iter()
            .map(|line| line.line)
            .collect::<Vec<_>>();
        assert_eq!(retained, vec!["before restart", "late first run"]);
        Ok(())
    }

    #[test]
    fn late_log_for_evicted_run_is_dropped_instead_of_resurrecting_it() {
        let retention = LogRetention {
            disk: DiskLogRetention { retained_runs: 2 },
            ..LogRetention::default()
        };
        let (reader, writer) = new_with_retention(
            [(snapshot("svc"), retention)],
            Some(unique_spool_dir("evicted-late-log")),
            None,
        );
        let id = "svc".to_string();

        writer.begin_run(&id, 1);
        let first_run = writer.run_sink(&id, 1);
        for run in 1..=3 {
            writer.begin_run(&id, run);
            writer.append_log(
                &id,
                run,
                OutputStream::Stdout,
                LogUpdateKind::Append,
                format!("run {run}"),
            );
        }

        first_run.append_log(
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "late line from evicted run".to_string(),
        );

        let runs: Vec<u64> = reader
            .log_runs(&id)
            .into_iter()
            .map(|run| run.run_generation)
            .collect();
        assert_eq!(runs, vec![2, 3]);
        assert!(reader.run_log(&id, 1, None).is_none());
    }

    #[test]
    fn retained_run_ring_evicts_oldest_runs() {
        let retention = LogRetention {
            disk: DiskLogRetention { retained_runs: 2 },
            ..LogRetention::default()
        };
        let (reader, writer) = new_with_retention(
            [(snapshot("svc"), retention)],
            Some(unique_spool_dir("retained-ring")),
            None,
        );
        let id = "svc".to_string();

        for run in 1..=3 {
            writer.begin_run(&id, run);
            writer.append_log(
                &id,
                run,
                OutputStream::Stdout,
                LogUpdateKind::Append,
                format!("run {run}"),
            );
        }

        let runs: Vec<u64> = reader
            .log_runs(&id)
            .into_iter()
            .map(|run| run.run_generation)
            .collect();
        assert_eq!(runs, vec![2, 3]);
        assert!(reader.run_log(&id, 1, None).is_none());
        assert!(reader.run_log(&id, 2, None).is_some());
    }

    #[test]
    fn run_log_after_pages_from_the_beginning_while_run_log_returns_the_tail() {
        // A run with more records than fit one page (the control plane caps a page at
        // MAX_LOG_TAIL = 2000): follow paging must reach the front, not just the tail.
        const TOTAL: usize = 2500;
        const PAGE: usize = 2000;

        let (reader, writer) = new_with_retention(
            [entry("svc")],
            Some(unique_spool_dir("follow-paging")),
            None,
        );
        let id = "svc".to_string();

        writer.begin_run(&id, 1);
        for n in 1..=TOTAL {
            writer.append_log(
                &id,
                1,
                OutputStream::Stdout,
                LogUpdateKind::Append,
                n.to_string(),
            );
        }

        // run_log_after(after = None) starts at the very beginning of the run.
        let head = reader
            .run_log_after(&id, 1, None, Some(PAGE))
            .expect("run exists");
        let head_lines: Vec<&str> = head.lines.iter().map(|line| line.line.as_str()).collect();
        assert_eq!(head_lines.first().copied(), Some("1"));
        assert!(!head_lines.contains(&"2500"));

        // run_log(tail) returns the end of the run instead.
        let tail = reader.run_log(&id, 1, Some(PAGE)).expect("run exists");
        let tail_lines: Vec<&str> = tail.lines.iter().map(|line| line.line.as_str()).collect();
        assert!(tail_lines.contains(&"2500"));
        assert!(!tail_lines.contains(&"1"));

        // Paging forward from the head cursor reaches the end of the run.
        let cursor = head
            .lines
            .last()
            .map(|line| line.seq)
            .expect("non-empty head");
        let next = reader
            .run_log_after(&id, 1, Some(cursor), Some(PAGE))
            .expect("run exists");
        assert!(next.lines.iter().any(|line| line.line == "2500"));
    }

    #[test]
    fn disk_unavailable_does_not_advertise_unqueryable_runs() {
        let (reader, writer) = new_with_retention([entry("svc")], None, None);
        let id = "svc".to_string();

        writer.begin_run(&id, 1);
        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "visible".into(),
        );

        let visible: Vec<String> = reader.logs(&id, None).into_iter().map(|l| l.line).collect();
        assert_eq!(visible, vec!["visible".to_string()]);
        assert!(reader.log_runs(&id).is_empty());
        assert!(reader.run_log(&id, 1, None).is_none());
    }

    #[test]
    fn empty_started_run_is_listed_and_queryable() {
        let (reader, writer) = new_with_retention(
            [entry("svc")],
            Some(unique_spool_dir("empty-started-run")),
            None,
        );
        let id = "svc".to_string();
        let mut snap = snapshot("svc");
        snap.run_generation = 1;
        snap.execution = Execution::Exited;
        snap.last_exit_code = Some(-1);

        writer.write_snapshot(snap, None);
        writer.begin_run(&id, 1);

        let runs = reader.log_runs(&id);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs.first().map(|run| run.current), Some(true));
        assert_eq!(runs.first().map(|run| run.line_count), Some(0));
        assert!(
            reader
                .run_log(&id, 1, None)
                .is_some_and(|run| run.lines.is_empty())
        );
    }

    #[test]
    fn failed_spawn_generation_is_synthetic_and_does_not_evict_retained_logs() {
        let retention = LogRetention {
            disk: DiskLogRetention { retained_runs: 2 },
            ..LogRetention::default()
        };
        let (reader, writer) = new_with_retention(
            [(snapshot("svc"), retention)],
            Some(unique_spool_dir("failed-spawn-synthetic")),
            None,
        );
        let id = "svc".to_string();

        for run in 1..=2 {
            writer.begin_run(&id, run);
            writer.append_log(
                &id,
                run,
                OutputStream::Stdout,
                LogUpdateKind::Append,
                format!("run {run}"),
            );
        }

        let mut snap = snapshot("svc");
        snap.run_generation = 3;
        snap.execution = Execution::Exited;
        snap.last_exit_code = Some(-1);
        writer.write_snapshot(snap, None);

        let runs = reader.log_runs(&id);
        assert_eq!(
            runs.iter()
                .map(|run| run.run_generation)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(runs.iter().filter(|run| run.current).count(), 1);
        assert_eq!(runs.last().map(|run| run.line_count), Some(0));
        assert_eq!(
            reader.run_log(&id, 1, None).map(|run| run
                .lines
                .into_iter()
                .map(|line| line.line)
                .collect::<Vec<_>>()),
            Some(vec!["run 1".to_string()])
        );
        assert_eq!(
            reader.run_log(&id, 2, None).map(|run| run
                .lines
                .into_iter()
                .map(|line| line.line)
                .collect::<Vec<_>>()),
            Some(vec!["run 2".to_string()])
        );
        assert!(
            reader
                .run_log(&id, 3, None)
                .is_some_and(|run| run.current && run.lines.is_empty())
        );
    }

    #[test]
    fn zero_disk_retention_is_clamped_at_model_boundary() {
        let retention = LogRetention {
            disk: DiskLogRetention { retained_runs: 0 },
            ..LogRetention::default()
        };
        let (reader, writer) = new_with_retention(
            [(snapshot("svc"), retention)],
            Some(unique_spool_dir("zero-retention")),
            None,
        );
        let id = "svc".to_string();

        writer.begin_run(&id, 1);
        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "line".into(),
        );

        assert_eq!(reader.logs(&id, None).len(), 1);
        assert_eq!(reader.log_runs(&id).len(), 1);
    }

    #[test]
    fn disk_run_log_keeps_full_run_when_memory_tail_is_bounded() {
        let retention = LogRetention {
            memory: MemoryLogRetention {
                max_lines: LogLimit::Bounded(1),
                max_bytes: LogLimit::Unbounded,
            },
            disk: DiskLogRetention { retained_runs: 2 },
        };
        let (reader, writer) = new_with_retention(
            [(snapshot("svc"), retention)],
            Some(unique_spool_dir("disk-full-run")),
            None,
        );
        let id = "svc".to_string();
        writer.begin_run(&id, 1);
        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "first".into(),
        );
        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "second".into(),
        );

        let visible: Vec<String> = reader.logs(&id, None).into_iter().map(|l| l.line).collect();
        assert_eq!(visible, vec!["second".to_string()]);
        let full: Vec<String> = reader
            .run_log(&id, 1, None)
            .expect("run retained on disk")
            .lines
            .into_iter()
            .map(|l| l.line)
            .collect();
        assert_eq!(full, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn disk_run_logs_do_not_collide_after_path_encoding() {
        let (reader, writer) = new_with_retention(
            [entry("a/b"), entry("a_b")],
            Some(unique_spool_dir("disk-collision")),
            None,
        );
        let slash = "a/b".to_string();
        let underscore = "a_b".to_string();

        writer.begin_run(&slash, 1);
        writer.append_log(
            &slash,
            1,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "slash".into(),
        );
        writer.begin_run(&underscore, 1);
        writer.append_log(
            &underscore,
            1,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "underscore".into(),
        );

        let slash_log: Vec<String> = reader
            .run_log(&slash, 1, None)
            .expect("slash service retained")
            .lines
            .into_iter()
            .map(|line| line.line)
            .collect();
        let underscore_log: Vec<String> = reader
            .run_log(&underscore, 1, None)
            .expect("underscore service retained")
            .lines
            .into_iter()
            .map(|line| line.line)
            .collect();

        assert_eq!(slash_log, vec!["slash".to_string()]);
        assert_eq!(underscore_log, vec!["underscore".to_string()]);
    }

    #[test]
    fn retained_run_cursor_read_returns_bounded_page() {
        let (reader, writer) =
            new_with_retention([entry("svc")], Some(unique_spool_dir("disk-cursor")), None);
        let id = "svc".to_string();

        writer.begin_run(&id, 1);
        for idx in 0..5 {
            writer.append_log(
                &id,
                1,
                OutputStream::Stdout,
                LogUpdateKind::Append,
                format!("line {idx}"),
            );
        }

        let page: Vec<String> = reader
            .run_log_after(&id, 1, Some(2), Some(2))
            .expect("run retained")
            .lines
            .into_iter()
            .map(|line| line.line)
            .collect();
        assert_eq!(page, vec!["line 2".to_string(), "line 3".to_string()]);
    }

    #[test]
    fn retained_run_cursor_read_reuses_scanned_offsets() {
        let (reader, writer) = new_with_retention(
            [entry("svc")],
            Some(unique_spool_dir("disk-cursor-index")),
            None,
        );
        let id = "svc".to_string();

        writer.begin_run(&id, 1);
        for idx in 0..6 {
            writer.append_log(
                &id,
                1,
                OutputStream::Stdout,
                LogUpdateKind::Append,
                format!("line {idx}"),
            );
        }

        let first_page = reader.run_log_after(&id, 1, Some(2), Some(2));
        assert_eq!(
            first_page.map(|run| run
                .lines
                .into_iter()
                .map(|line| line.line)
                .collect::<Vec<_>>()),
            Some(vec!["line 2".to_string(), "line 3".to_string()])
        );
        let scanned_after_first_read = {
            let guard = reader.inner.services.read();
            guard
                .get(&id)
                .and_then(|entry| entry.runs.front())
                .map(|run| run.read_index.scanned_to)
        };

        let second_page = reader.run_log_after(&id, 1, Some(4), Some(1));
        assert_eq!(
            second_page.map(|run| run
                .lines
                .into_iter()
                .map(|line| line.line)
                .collect::<Vec<_>>()),
            Some(vec!["line 4".to_string()])
        );
        let scanned_after_second_read = {
            let guard = reader.inner.services.read();
            guard
                .get(&id)
                .and_then(|entry| entry.runs.front())
                .map(|run| run.read_index.scanned_to)
        };
        assert!(
            scanned_after_second_read
                .zip(scanned_after_first_read)
                .is_some_and(|(after_second, after_first)| after_second > after_first)
        );
        assert!(
            reader
                .run_log_after(&id, 1, Some(99), Some(10))
                .is_some_and(|run| run.lines.is_empty())
        );
    }

    #[test]
    fn retained_run_tail_uses_index_without_changing_results() {
        let (reader, writer) = new_with_retention(
            [entry("svc")],
            Some(unique_spool_dir("disk-tail-index")),
            None,
        );
        let id = "svc".to_string();

        writer.begin_run(&id, 1);
        for idx in 1..=50 {
            writer.append_log(
                &id,
                1,
                OutputStream::Stdout,
                LogUpdateKind::Append,
                idx.to_string(),
            );
        }

        let cold_tail = reader
            .run_log(&id, 1, Some(10))
            .expect("run retained")
            .lines
            .into_iter()
            .map(|line| line.line)
            .collect::<Vec<_>>();
        assert_eq!(
            cold_tail,
            (41..=50).map(|idx| idx.to_string()).collect::<Vec<_>>()
        );

        assert!(
            reader
                .run_log_after(&id, 1, Some(25), Some(10))
                .is_some_and(|run| !run.lines.is_empty())
        );
        let warm_tail = reader
            .run_log(&id, 1, Some(10))
            .expect("run retained")
            .lines
            .into_iter()
            .map(|line| line.line)
            .collect::<Vec<_>>();
        assert_eq!(warm_tail, cold_tail);
    }

    #[test]
    fn retained_run_cursor_below_offset_cache_window_does_not_skip_lines() {
        let (reader, writer) = new_with_retention(
            [entry("svc")],
            Some(unique_spool_dir("disk-cursor-low")),
            None,
        );
        let id = "svc".to_string();

        writer.begin_run(&id, 1);
        for idx in 0..(RUN_LOG_OFFSET_CACHE + 128) {
            writer.append_log(
                &id,
                1,
                OutputStream::Stdout,
                LogUpdateKind::Append,
                format!("line {idx}"),
            );
        }

        assert!(
            reader
                .run_log(&id, 1, Some(1))
                .is_some_and(|run| run.lines.len() == 1)
        );

        let page = reader.run_log_after(&id, 1, Some(11), Some(3));
        assert_eq!(
            page.map(|run| run
                .lines
                .into_iter()
                .map(|line| line.line)
                .collect::<Vec<_>>()),
            Some(vec![
                "line 11".to_string(),
                "line 12".to_string(),
                "line 13".to_string(),
            ])
        );
    }

    #[test]
    fn model_drop_removes_session_spool_dir() {
        let spool = unique_spool_dir("spool-cleanup");
        let (reader, writer) = new_with_retention([entry("svc")], Some(spool.clone()), None);
        let id = "svc".to_string();
        writer.begin_run(&id, 1);
        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "line".into(),
        );
        assert!(reader.run_log(&id, 1, None).is_some());
        assert!(spool.exists());

        drop(writer);
        drop(reader);

        assert!(!spool.exists());
    }

    #[tokio::test]
    async fn subscribe_observes_a_status_change() {
        let (reader, writer) = new([entry("svc")]);
        let mut rx = reader.subscribe();
        let mut snap = snapshot("svc");
        snap.execution = Execution::Running;
        writer.write_snapshot(snap, Some(Instant::now()));

        let change = rx.recv().await.expect("a change");
        assert_eq!(change.service_id, "svc".to_string());
        assert_eq!(change.kind, ChangeKind::Status);

        // Re-query reflects the new content.
        let services = reader.services();
        assert_eq!(
            services.first().map(|s| s.execution),
            Some(Execution::Running)
        );
        assert!(services.first().and_then(|s| s.uptime).is_some());
    }

    #[tokio::test]
    async fn roster_insert_and_remove_publish_changes_in_order() {
        let (reader, writer) = new([entry("configured")]);
        let mut changes = reader.subscribe();
        writer.insert_service(snapshot("dynamic"), LogRetention::default());

        let inserted = changes.recv().await.expect("insert change");
        assert_eq!(inserted.service_id, "dynamic");
        assert_eq!(inserted.kind, ChangeKind::Roster);
        assert_eq!(
            reader
                .services()
                .into_iter()
                .map(|snapshot| snapshot.id)
                .collect::<Vec<_>>(),
            vec!["configured", "dynamic"]
        );

        writer.remove_service(&"dynamic".to_string());
        let removed = changes.recv().await.expect("remove change");
        assert_eq!(removed.service_id, "dynamic");
        assert_eq!(removed.kind, ChangeKind::Roster);
        assert!(reader.service("dynamic").is_none());
    }

    #[test]
    fn roster_removal_deletes_retained_run_files() -> color_eyre::Result<()> {
        let spool = unique_spool_dir("removed-service");
        let (reader, writer) = new_with_retention([entry("dynamic")], Some(spool.clone()), None);
        let id = "dynamic".to_string();
        writer.begin_run(&id, 1);
        writer.append_log(
            &id,
            1,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "retained".to_string(),
        );
        let path = reader
            .log_runs(&id)
            .into_iter()
            .find_map(|run| run.path)
            .map(PathBuf::from)
            .ok_or_else(|| color_eyre::eyre::eyre!("missing retained run path"))?;
        let _ = reader.run_log(&id, 1, None);
        assert!(path.exists());

        writer.remove_service(&id);
        writer.inner.flush_disk();

        assert!(!path.exists());
        drop(reader);
        drop(writer);
        assert!(!spool.exists());
        Ok(())
    }

    #[test]
    fn service_event_history_is_bounded_and_pages_by_cursor_or_tail() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();
        for generation in 1..=EVENT_HISTORY as u64 + 5 {
            writer.append_event(
                &id,
                ServiceEvent {
                    seq: 0,
                    at_unix_ms: generation,
                    run_generation: generation,
                    kind: ServiceEventKind::Spawned,
                    detail: format!("generation {generation}"),
                    exit_code: None,
                    pid: None,
                    delay_ms: None,
                    blocked_on: None,
                },
            );
        }

        let (history, truncated) = reader.events(&id, None, None);
        assert_eq!(history.len(), EVENT_HISTORY);
        assert!(truncated);
        assert_eq!(history.first().map(|event| event.seq), Some(6));
        assert_eq!(history.last().map(|event| event.seq), Some(261));

        let (tail, truncated) = reader.events(&id, None, Some(3));
        assert_eq!(
            tail.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![259, 260, 261]
        );
        assert!(truncated);

        let (page, truncated) = reader.events(&id, Some(257), Some(2));
        assert_eq!(
            page.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![258, 259]
        );
        assert!(truncated);
    }

    #[test]
    fn healthcheck_history_is_bounded_and_records_results() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();
        writer.write_snapshot(running_snapshot(&id), Some(Instant::now()));
        writer.begin_run(&id, 1);
        for attempt in 1..=(HEALTH_HISTORY as u64 + 2) {
            writer.start_health_attempt(&id, 1, attempt, format!("probe {attempt}"));
            writer.finish_health_attempt(&id, 1, attempt, true, 0, false);
        }
        let history = reader.healthchecks(&id);
        assert_eq!(history.len(), HEALTH_HISTORY);
        assert!(history.iter().all(|attempt| attempt.run_generation == 1));
        assert_eq!(
            reader
                .latest_health(&id)
                .and_then(|a| a.result)
                .map(|r| r.success),
            Some(true)
        );
    }

    #[test]
    fn begin_run_clears_stale_healthcheck_history() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();

        writer.write_snapshot(running_snapshot(&id), Some(Instant::now()));
        writer.begin_run(&id, 1);
        writer.start_health_attempt(&id, 1, 1, "probe old".to_string());
        writer.finish_health_attempt(&id, 1, 1, false, 1, false);
        assert!(reader.latest_health(&id).is_some());

        writer.begin_run(&id, 2);
        assert!(reader.latest_health(&id).is_none());
        assert!(reader.healthchecks(&id).is_empty());
    }

    #[test]
    fn latest_health_is_current_only_but_history_remains_after_exit() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();

        writer.write_snapshot(running_snapshot(&id), Some(Instant::now()));
        writer.begin_run(&id, 1);
        writer.start_health_attempt(&id, 1, 1, "probe".to_string());
        writer.finish_health_attempt(&id, 1, 1, true, 0, false);
        assert!(reader.latest_health(&id).is_some());

        let mut exited = running_snapshot(&id);
        exited.execution = Execution::Exited;
        exited.health = None;
        exited.last_exit_code = Some(1);
        writer.write_snapshot(exited, None);

        assert!(reader.latest_health(&id).is_none());
        let history = reader.healthchecks(&id);
        assert_eq!(history.len(), 1);
        let Some(attempt) = history.first() else {
            panic!("expected one historical healthcheck attempt");
        };
        assert_eq!(attempt.run_generation, 1);
        assert_eq!(attempt.result.map(|result| result.success), Some(true));
    }

    #[test]
    fn latest_health_must_match_current_run_generation() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();

        writer.write_snapshot(running_snapshot(&id), Some(Instant::now()));
        writer.begin_run(&id, 1);
        writer.start_health_attempt(&id, 1, 1, "probe stale".to_string());
        writer.finish_health_attempt(&id, 1, 1, true, 0, false);
        assert!(reader.latest_health(&id).is_some());

        let mut next_run = running_snapshot(&id);
        next_run.run_generation = 2;
        writer.write_snapshot(next_run, Some(Instant::now()));

        assert!(reader.latest_health(&id).is_none());
        assert_eq!(reader.healthchecks(&id).len(), 1);
    }

    #[test]
    fn stale_health_attempt_is_ignored_after_new_run_begins() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();

        let mut current = running_snapshot(&id);
        current.run_generation = 2;
        writer.write_snapshot(current, Some(Instant::now()));
        writer.begin_run(&id, 2);
        writer.start_health_attempt(&id, 2, 1, "probe current".to_string());
        writer.finish_health_attempt(&id, 2, 1, true, 0, false);
        writer.start_health_attempt(&id, 1, 2, "probe old".to_string());
        writer.finish_health_attempt(&id, 1, 2, false, 1, false);

        let Some(attempt) = reader.latest_health(&id) else {
            panic!("expected current-run healthcheck attempt");
        };
        assert_eq!(attempt.run_generation, 2);
        assert_eq!(attempt.command, "probe current");
        assert_eq!(reader.healthchecks(&id).len(), 1);
    }

    #[test]
    fn health_updates_must_match_run_generation_and_attempt() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();

        let mut current = running_snapshot(&id);
        current.run_generation = 2;
        writer.write_snapshot(current, Some(Instant::now()));
        writer.begin_run(&id, 2);
        writer.start_health_attempt(&id, 2, 1, "probe current".to_string());
        writer.start_health_attempt(&id, 1, 1, "probe stale".to_string());

        writer.append_health_line(&id, 1, 1, OutputStream::Stderr, "stale".to_string());
        let Some(attempt) = reader.latest_health(&id) else {
            panic!("expected current-run healthcheck attempt");
        };
        assert!(attempt.output.is_empty());

        writer.append_health_line(&id, 2, 1, OutputStream::Stdout, "current".to_string());
        let Some(attempt) = reader.latest_health(&id) else {
            panic!("expected current-run healthcheck attempt");
        };
        assert_eq!(
            attempt.output.first().map(|line| line.line.as_str()),
            Some("current")
        );

        writer.finish_health_attempt(&id, 1, 1, false, 1, false);
        let Some(attempt) = reader.latest_health(&id) else {
            panic!("expected current-run healthcheck attempt");
        };
        assert_eq!(attempt.result.as_ref().map(|result| result.success), None);

        writer.finish_health_attempt(&id, 2, 1, true, 0, false);
        let Some(attempt) = reader.latest_health(&id) else {
            panic!("expected current-run healthcheck attempt");
        };
        assert_eq!(attempt.result.map(|result| result.success), Some(true));
    }
}
