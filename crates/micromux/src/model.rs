//! The authoritative session model.
//!
//! The scheduler owns the lifecycle truth; this module materializes it as a queryable model that
//! every frontend (the TUI, the control socket, the MCP server) reads from. There is exactly one
//! source of truth: the scheduler *writes* it through a possession-scoped [`SessionModelWriter`],
//! and adapters *read* it through a cloneable [`SessionModelReader`]. The lack of a write method on
//! the reader is the security boundary — an adapter that only holds a reader can observe but never
//! mutate, and the only path by which it can affect state is the narrow
//! [`crate::ServiceControl`] command port, which the scheduler validates before writing anything.
//!
//! `Inner` is behind a synchronous [`parking_lot::RwLock`]; readers snapshot/clone under the lock,
//! drop it, and only then serialize — never holding the guard across an `.await`.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use parking_lot::RwLock;
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

/// Capacity of the liveness-only change broadcast. A lagging subscriber loses only coalescible
/// notifications (it re-queries the model for content), never log bytes.
const CHANGE_CHANNEL_CAPACITY: usize = 1024;
const RUN_LOG_OFFSET_CACHE: usize = 4096;

/// The state a service has been *asked* to be in. `Disabled` is a desire, not an execution state —
/// a disabled service may still be draining.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Desired {
    /// The service should be running.
    Enabled,
    /// The service should be stopped and stay stopped.
    Disabled,
}

/// The observed lifecycle phase of a service, independent of whether it is `Desired::Enabled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
}

/// A point-in-time, serializable view of one service. This is the wire payload reused directly by
/// the control protocol (no DTO mirror).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceSnapshot {
    /// Stable service identifier.
    pub id: ServiceID,
    /// Human-readable service name.
    pub name: String,
    /// Requested state (`Disabled` is a desire, not an execution).
    pub desired: Desired,
    /// Observed lifecycle phase.
    pub execution: Execution,
    /// Latest resolved health, if a healthcheck is configured and has produced a verdict.
    pub health: Option<Health>,
    /// Public name for the scheduler's `RunId`; bumps on every successful (re)start. `0` means the
    /// service has never started.
    pub run_generation: u64,
    /// Parsed open ports.
    pub open_ports: Vec<u16>,
    /// Whether this service has a healthcheck configured.
    pub healthcheck_configured: bool,
    /// Exit code of the most recently finished run, if any.
    pub last_exit_code: Option<i32>,
    /// Time since the current run started, refreshed at read time. `None` when not running.
    pub uptime: Option<Duration>,
    /// The configured restart policy.
    pub restart_policy: RestartPolicy,
}

impl ServiceSnapshot {
    /// Construct the initial model snapshot for a configured service before the scheduler has
    /// attempted to start it.
    #[must_use]
    pub fn initial(
        id: ServiceID,
        name: String,
        open_ports: Vec<u16>,
        healthcheck_configured: bool,
        restart_policy: RestartPolicy,
    ) -> Self {
        Self {
            id,
            name,
            desired: Desired::Enabled,
            execution: Execution::Pending,
            health: None,
            run_generation: 0,
            open_ports,
            healthcheck_configured,
            last_exit_code: None,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    /// Monotonic sequence number; survives eviction so a follower can resume without gaps or dupes.
    pub seq: u64,
    /// The service run that produced this line.
    pub run_generation: u64,
    /// The already-formatted line (stderr lines carry a `[stderr]` prefix, matching the TUI).
    pub line: String,
}

/// Summary of one retained service run's logs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRunSummary {
    /// The scheduler run generation for this service run.
    pub run_generation: u64,
    /// Whether this is the latest known run for the service.
    pub current: bool,
    /// Number of retained lines for this run.
    pub line_count: usize,
    /// Sequence number of the first retained line, if any.
    pub first_seq: Option<u64>,
    /// Sequence number of the last retained line, if any.
    pub last_seq: Option<u64>,
}

/// Retained logs for one service run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRun {
    /// The scheduler run generation for this service run.
    pub run_generation: u64,
    /// Whether this is the latest known run for the service.
    pub current: bool,
    /// Retained lines for the run.
    pub lines: Vec<LogLine>,
}

/// One healthcheck output line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthLine {
    /// Which stream produced the output.
    pub stream: OutputStream,
    /// The output line.
    pub line: String,
}

/// The result of a finished healthcheck attempt.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HealthResult {
    /// Whether the probe exited successfully.
    pub success: bool,
    /// Exit code of the probe process.
    pub exit_code: i32,
}

/// One healthcheck attempt and its (bounded) output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthAttempt {
    /// Monotonic attempt number within the current run.
    pub attempt: u64,
    /// The command that was executed for this probe.
    pub command: String,
    /// Captured probe output (bounded).
    pub output: Vec<HealthLine>,
    /// The final result, or `None` while the probe is still running.
    pub result: Option<HealthResult>,
}

/// What kind of change a [`SessionChange`] notification coalesces. The broadcast is liveness-only:
/// subscribers receive the kind and re-query the model for content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    /// The service's status snapshot changed.
    Status,
    /// New log content is available.
    Logs,
    /// Healthcheck history changed.
    Health,
}

/// A liveness-only change notification. `broadcast` drops for lagging receivers, so this must never
/// be the carrier of content — subscribers re-query the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionChange {
    /// The service that changed.
    pub service_id: ServiceID,
    /// What changed.
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum DiskLogOp {
    Append,
    ReplaceLast,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiskLogRecord {
    seq: u64,
    run_generation: u64,
    op: DiskLogOp,
    line: String,
}

enum DiskLogCommand {
    Begin {
        path: PathBuf,
    },
    Write {
        path: PathBuf,
        record: DiskLogRecord,
    },
    Remove {
        path: PathBuf,
    },
    Flush {
        done: mpsc::Sender<()>,
    },
}

#[derive(Clone)]
struct DiskLogWriter {
    tx: mpsc::Sender<DiskLogCommand>,
}

impl DiskLogWriter {
    fn begin(&self, path: PathBuf) {
        let _ = self.tx.send(DiskLogCommand::Begin { path });
    }

    fn write(&self, path: PathBuf, record: DiskLogRecord) {
        let _ = self.tx.send(DiskLogCommand::Write { path, record });
    }

    fn remove(&self, path: PathBuf) {
        let _ = self.tx.send(DiskLogCommand::Remove { path });
    }
}

struct DiskLogWorker {
    tx: Option<mpsc::Sender<DiskLogCommand>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl DiskLogWorker {
    fn spawn() -> (Self, DiskLogWriter) {
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || run_disk_log_worker(rx));
        (
            Self {
                tx: Some(tx.clone()),
                handle: Some(handle),
            },
            DiskLogWriter { tx },
        )
    }

    fn shutdown(&mut self) {
        self.tx.take();
        if let Some(handle) = self.handle.take()
            && let Err(err) = handle.join()
        {
            tracing::debug!(?err, "disk log worker panicked during shutdown");
        }
    }

    fn flush(&self) {
        let Some(tx) = &self.tx else {
            return;
        };
        let (done, wait) = mpsc::channel();
        if tx.send(DiskLogCommand::Flush { done }).is_ok() {
            let _ = wait.recv();
        }
    }
}

impl Drop for DiskLogWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn open_disk_log_writer(path: &Path, truncate: bool) -> Option<BufWriter<File>> {
    if let Some(parent) = path.parent()
        && let Err(err) = fs::create_dir_all(parent)
    {
        tracing::warn!(?err, path = %parent.display(), "failed to create log spool dir");
        return None;
    }

    let mut options = OpenOptions::new();
    options.create(true).write(true);
    if truncate {
        options.truncate(true);
    } else {
        options.append(true);
    }

    match options.open(path) {
        Ok(file) => Some(BufWriter::new(file)),
        Err(err) => {
            tracing::warn!(?err, path = %path.display(), "failed to open run log file");
            None
        }
    }
}

fn write_disk_record(
    writers: &mut HashMap<PathBuf, BufWriter<File>>,
    path: &Path,
    record: &DiskLogRecord,
) {
    if !writers.contains_key(path) {
        let Some(writer) = open_disk_log_writer(path, false) else {
            return;
        };
        writers.insert(path.to_path_buf(), writer);
    }

    let Some(writer) = writers.get_mut(path) else {
        return;
    };
    let result = serde_json::to_writer(&mut *writer, record)
        .map_err(std::io::Error::other)
        .and_then(|()| writer.write_all(b"\n"));
    if let Err(err) = result {
        tracing::warn!(?err, path = %path.display(), "disabling disk run log writer after write failure");
        writers.remove(path);
    }
}

fn run_disk_log_worker(rx: mpsc::Receiver<DiskLogCommand>) {
    let mut writers = HashMap::new();
    for command in rx {
        match command {
            DiskLogCommand::Begin { path } => {
                writers.remove(&path);
                if let Some(writer) = open_disk_log_writer(&path, true) {
                    writers.insert(path, writer);
                }
            }
            DiskLogCommand::Write { path, record } => {
                write_disk_record(&mut writers, &path, &record);
            }
            DiskLogCommand::Remove { path } => {
                writers.remove(&path);
                if let Err(err) = fs::remove_file(&path)
                    && err.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::debug!(?err, path = %path.display(), "failed to remove evicted run log");
                }
            }
            DiskLogCommand::Flush { done } => {
                for (path, writer) in &mut writers {
                    if let Err(err) = writer.flush() {
                        tracing::warn!(?err, path = %path.display(), "failed to flush run log");
                    }
                }
                let _ = done.send(());
            }
        }
    }
}

fn trim_to_last_bytes(line: String, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line;
    }
    if max_bytes == 0 {
        return String::new();
    }

    let min_start = line.len().saturating_sub(max_bytes);
    let start = line
        .char_indices()
        .map(|(idx, _)| idx)
        .find(|idx| *idx >= min_start)
        .unwrap_or(line.len());
    let mut line = line;
    line.split_off(start)
}

struct MemoryLogBuffer {
    entries: VecDeque<LogLine>,
    current_bytes: usize,
    retention: MemoryLogRetention,
}

impl MemoryLogBuffer {
    fn new(retention: MemoryLogRetention) -> Self {
        Self {
            entries: VecDeque::new(),
            current_bytes: 0,
            retention,
        }
    }

    fn push(&mut self, mut line: LogLine) {
        if let Some(max_bytes) = self.retention.max_bytes.as_option() {
            line.line = trim_to_last_bytes(line.line, max_bytes);
        }
        let line_len = line.line.len();

        if let Some(max_bytes) = self.retention.max_bytes.as_option() {
            while self.current_bytes.saturating_add(line_len) > max_bytes {
                let Some(old) = self.entries.pop_front() else {
                    break;
                };
                self.current_bytes = self.current_bytes.saturating_sub(old.line.len());
            }
        }

        self.entries.push_back(line);
        self.current_bytes = self.current_bytes.saturating_add(line_len);

        if let Some(max_lines) = self.retention.max_lines.as_option() {
            while self.entries.len() > max_lines {
                let Some(old) = self.entries.pop_front() else {
                    break;
                };
                self.current_bytes = self.current_bytes.saturating_sub(old.line.len());
            }
        }
    }

    fn append(&mut self, line: LogLine) {
        self.push(line);
    }

    fn replace_last(&mut self, line: LogLine) {
        if self.entries.back().is_some_and(|last| last.seq == line.seq) {
            if let Some(old) = self.entries.pop_back() {
                self.current_bytes = self.current_bytes.saturating_sub(old.line.len());
            }
            self.push(line);
        }
    }

    fn clear_before_next_seq(&mut self, next_seq: u64) {
        while self.entries.front().is_some_and(|line| line.seq < next_seq) {
            let Some(old) = self.entries.pop_front() else {
                break;
            };
            self.current_bytes = self.current_bytes.saturating_sub(old.line.len());
        }
    }

    fn lines(&self, tail: Option<usize>) -> Vec<LogLine> {
        let len = self.entries.len();
        let skip = tail.map_or(0, |n| len.saturating_sub(n));
        self.entries.iter().skip(skip).cloned().collect()
    }
}

#[derive(Clone, Debug, Default)]
struct RunLogReadIndex {
    scanned_to: u64,
    last_append_seq: Option<u64>,
    append_offsets: Vec<(u64, u64)>,
}

impl RunLogReadIndex {
    fn reset_if_past_end(&mut self, file_len: u64) {
        if self.scanned_to > file_len {
            self.scanned_to = 0;
            self.last_append_seq = None;
            self.append_offsets.clear();
        }
    }

    fn offset_at_or_before(&self, seq: u64) -> Option<u64> {
        let idx = self
            .append_offsets
            .partition_point(|(known_seq, _offset)| *known_seq <= seq);
        idx.checked_sub(1)
            .and_then(|idx| self.append_offsets.get(idx))
            .map(|(_seq, offset)| *offset)
    }

    fn observe_append(&mut self, seq: u64, offset: u64) {
        self.last_append_seq = Some(
            self.last_append_seq
                .map_or(seq, |last_seq| last_seq.max(seq)),
        );
        match self
            .append_offsets
            .binary_search_by_key(&seq, |(known_seq, _offset)| *known_seq)
        {
            Ok(_) => {}
            Err(idx) => self.append_offsets.insert(idx, (seq, offset)),
        }
        while self.append_offsets.len() > RUN_LOG_OFFSET_CACHE {
            self.append_offsets.remove(0);
        }
    }

    fn mark_scanned_to(&mut self, offset: u64) {
        self.scanned_to = self.scanned_to.max(offset);
    }
}

struct RunLogEntry {
    run_generation: u64,
    path: Option<PathBuf>,
    line_count: usize,
    first_seq: Option<u64>,
    last_seq: Option<u64>,
    live_snapshot_id: Option<u64>,
    read_index: RunLogReadIndex,
}

impl RunLogEntry {
    fn new(
        service_id: &ServiceID,
        run_generation: u64,
        spool_dir: Option<&Path>,
        disk: Option<&DiskLogWriter>,
    ) -> Self {
        let path = spool_dir.map(|dir| {
            dir.join(service_log_dir_name(service_id))
                .join(format!("run-{run_generation}.jsonl"))
        });
        if let (Some(path), Some(disk)) = (&path, disk) {
            disk.begin(path.clone());
        }

        Self {
            run_generation,
            path,
            line_count: 0,
            first_seq: None,
            last_seq: None,
            live_snapshot_id: None,
            read_index: RunLogReadIndex::default(),
        }
    }

    fn summary(&self, current: bool) -> LogRunSummary {
        LogRunSummary {
            run_generation: self.run_generation,
            current,
            line_count: self.line_count,
            first_seq: self.first_seq,
            last_seq: self.last_seq,
        }
    }

    fn append_metadata(&mut self, seq: u64) {
        self.line_count = self.line_count.saturating_add(1);
        self.first_seq.get_or_insert(seq);
        self.last_seq = Some(seq);
    }

    fn replace_metadata(&mut self, seq: u64) {
        if self.line_count == 0 {
            self.append_metadata(seq);
        } else {
            self.last_seq = Some(seq);
        }
    }

    fn enqueue_write(&self, disk: Option<&DiskLogWriter>, record: DiskLogRecord) {
        if let (Some(path), Some(disk)) = (&self.path, disk) {
            disk.write(path.clone(), record);
        }
    }

    fn enqueue_remove(&mut self, disk: Option<&DiskLogWriter>) {
        if let (Some(path), Some(disk)) = (self.path.take(), disk) {
            disk.remove(path);
        }
    }
}

fn service_log_dir_name(value: &str) -> String {
    if value.is_empty() {
        return "_".to_string();
    }

    let mut out = String::with_capacity(value.len() * 2);
    for byte in value.bytes() {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn create_spool_dir() -> Option<PathBuf> {
    let Some(base) = crate::project_dir().map(|dirs| dirs.cache_dir().join("session-run-logs"))
    else {
        tracing::warn!("disk run logs disabled: no project cache directory is available");
        return None;
    };
    if let Err(err) = fs::create_dir_all(&base) {
        tracing::warn!(?err, path = %base.display(), "disk run logs disabled");
        return None;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = base.join(format!("{}-{nanos}", std::process::id()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        if let Err(err) = fs::DirBuilder::new().mode(0o700).create(&path) {
            tracing::warn!(?err, path = %path.display(), "disk run logs disabled");
            return None;
        }
    }
    #[cfg(not(unix))]
    if let Err(err) = fs::create_dir(&path) {
        tracing::warn!(?err, path = %path.display(), "disk run logs disabled");
        return None;
    }
    Some(path)
}

fn push_log_line(
    lines: &mut VecDeque<LogLine>,
    line: LogLine,
    tail: Option<usize>,
    limit: Option<usize>,
) {
    if tail == Some(0) || limit == Some(0) {
        return;
    }
    if limit.is_some_and(|limit| lines.len() >= limit) {
        return;
    }
    lines.push_back(line);
    if let Some(tail) = tail {
        while lines.len() > tail {
            lines.pop_front();
        }
    }
}

fn start_offset_for_read(
    index: &RunLogReadIndex,
    tail: Option<usize>,
    after: Option<u64>,
    file_len: u64,
) -> u64 {
    if tail.is_some() {
        return 0;
    }
    let Some(after) = after else {
        return 0;
    };
    if index.last_append_seq.is_some_and(|seq| seq <= after) {
        return index.scanned_to.min(file_len);
    }
    index.offset_at_or_before(after).unwrap_or(0)
}

fn read_run_log_file(
    path: &Path,
    tail: Option<usize>,
    after: Option<u64>,
    limit: Option<usize>,
    mut index: RunLogReadIndex,
) -> Option<(Vec<LogLine>, RunLogReadIndex)> {
    let Ok(file) = File::open(path) else {
        return None;
    };
    let file_len = file.metadata().ok()?.len();
    index.reset_if_past_end(file_len);
    let start_offset = start_offset_for_read(&index, tail, after, file_len);
    if start_offset == 0 {
        index = RunLogReadIndex::default();
    }

    let mut reader = BufReader::new(file);
    if reader.seek(SeekFrom::Start(start_offset)).is_err() {
        return None;
    }

    let mut offset = start_offset;
    let mut lines = VecDeque::new();
    loop {
        let record_offset = offset;
        let mut raw = Vec::new();
        let bytes_read = match reader.read_until(b'\n', &mut raw) {
            Ok(0) => break,
            Ok(bytes_read) => bytes_read,
            Err(err) => {
                tracing::debug!(?err, path = %path.display(), "skipping unreadable run log record");
                break;
            }
        };
        offset = offset.saturating_add(u64::try_from(bytes_read).unwrap_or(u64::MAX));
        index.mark_scanned_to(offset);

        let Ok(raw) = std::str::from_utf8(&raw) else {
            tracing::debug!(path = %path.display(), "skipping non-utf8 run log record");
            continue;
        };
        let Ok(record) = serde_json::from_str::<DiskLogRecord>(raw) else {
            tracing::debug!(path = %path.display(), "skipping malformed run log record");
            continue;
        };
        if matches!(record.op, DiskLogOp::Append) {
            index.observe_append(record.seq, record_offset);
        }
        if after.is_some_and(|cursor| record.seq <= cursor) {
            continue;
        }
        if tail.is_none()
            && limit.is_some_and(|limit| lines.len() >= limit)
            && !matches!(record.op, DiskLogOp::ReplaceLast)
        {
            break;
        }
        let line = LogLine {
            seq: record.seq,
            run_generation: record.run_generation,
            line: record.line,
        };
        match record.op {
            DiskLogOp::Append => push_log_line(&mut lines, line, tail, limit),
            DiskLogOp::ReplaceLast => {
                if let Some(last) = lines.back_mut()
                    && last.seq == line.seq
                {
                    *last = line;
                } else if lines.is_empty() {
                    push_log_line(&mut lines, line, tail, limit);
                }
            }
        }
    }
    Some((lines.into_iter().collect(), index))
}

struct ServiceEntry {
    snapshot: ServiceSnapshot,
    started_at: Option<Instant>,
    visible: MemoryLogBuffer,
    runs: VecDeque<RunLogEntry>,
    next_log_seq: u64,
    log_retention: LogRetention,
    spool_dir: Option<PathBuf>,
    disk: Option<DiskLogWriter>,
    health: VecDeque<HealthAttempt>,
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
        let log_retention = LogRetention {
            disk: DiskLogRetention {
                retained_runs: log_retention.disk.retained_runs.max(1),
            },
            ..log_retention
        };
        Self {
            snapshot,
            started_at: None,
            visible: MemoryLogBuffer::new(log_retention.memory),
            runs: VecDeque::new(),
            next_log_seq: 0,
            log_retention,
            spool_dir: spool_dir.map(Path::to_path_buf),
            disk,
            health: VecDeque::new(),
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
        if let Some(run) = self.ensure_run(run_generation) {
            run.live_snapshot_id = None;
        }
        self.health.clear();
    }

    fn append_log(&mut self, run_generation: u64, update: LogUpdateKind, line: String) {
        let mut next_seq = self.next_log_seq;
        let disk = self.disk.clone();
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
                line,
            };
            let record = DiskLogRecord {
                seq,
                run_generation,
                op,
                line: line.line.clone(),
            };
            run.enqueue_write(disk.as_ref(), record);
            Some((op, line))
        }) else {
            return;
        };
        self.next_log_seq = next_seq;

        match op {
            DiskLogOp::Append => self.visible.append(line),
            DiskLogOp::ReplaceLast => self.visible.replace_last(line),
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
}

struct Inner {
    services: RwLock<IndexMap<ServiceID, ServiceEntry>>,
    change_tx: broadcast::Sender<SessionChange>,
    spool_dir: Option<PathBuf>,
    disk: Option<DiskLogWorker>,
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
        if let Some(disk) = self.disk.as_mut() {
            disk.shutdown();
        }
        if let Some(path) = self.spool_dir.take()
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

    /// The full retained healthcheck history for a service (oldest first).
    #[must_use]
    pub fn healthchecks(&self, id: &str) -> Vec<HealthAttempt> {
        let guard = self.inner.services.read();
        guard
            .get(id)
            .map(|entry| entry.health.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The most recent healthcheck attempt for a service, if any.
    #[must_use]
    pub fn latest_health(&self, id: &str) -> Option<HealthAttempt> {
        let guard = self.inner.services.read();
        guard.get(id).and_then(|entry| entry.health.back().cloned())
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

impl SessionModelWriter {
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
    pub(crate) fn append_log(
        &self,
        id: &ServiceID,
        run_generation: u64,
        stream: OutputStream,
        update: LogUpdateKind,
        line: String,
    ) {
        let line = match stream {
            OutputStream::Stdout => line,
            OutputStream::Stderr => format!("[stderr] {line}"),
        };
        {
            let mut guard = self.inner.services.write();
            let Some(entry) = guard.get_mut(id) else {
                return;
            };
            entry.append_log(run_generation, update, line);
        }
        self.inner.publish(id, ChangeKind::Logs);
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

    /// Begin a new healthcheck attempt, evicting the oldest beyond the retained history.
    pub(crate) fn start_health_attempt(&self, id: &ServiceID, attempt: u64, command: String) {
        {
            let mut guard = self.inner.services.write();
            let Some(entry) = guard.get_mut(id) else {
                return;
            };
            while entry.health.len() >= HEALTH_HISTORY {
                entry.health.pop_front();
            }
            entry.health.push_back(HealthAttempt {
                attempt,
                command,
                output: Vec::new(),
                result: None,
            });
        }
        self.inner.publish(id, ChangeKind::Health);
    }

    /// Append output to an in-progress healthcheck attempt.
    pub(crate) fn append_health_line(
        &self,
        id: &ServiceID,
        attempt: u64,
        stream: OutputStream,
        line: String,
    ) {
        {
            let mut guard = self.inner.services.write();
            let Some(entry) = guard.get_mut(id) else {
                return;
            };
            let Some(attempt_entry) = entry.health.iter_mut().find(|a| a.attempt == attempt) else {
                return;
            };
            while attempt_entry.output.len() >= HEALTH_OUTPUT_MAX_LINES {
                attempt_entry.output.remove(0);
            }
            attempt_entry.output.push(HealthLine { stream, line });
        }
        self.inner.publish(id, ChangeKind::Health);
    }

    /// Record the result of a finished healthcheck attempt.
    pub(crate) fn finish_health_attempt(
        &self,
        id: &ServiceID,
        attempt: u64,
        success: bool,
        exit_code: i32,
    ) {
        {
            let mut guard = self.inner.services.write();
            let Some(entry) = guard.get_mut(id) else {
                return;
            };
            let Some(attempt_entry) = entry.health.iter_mut().find(|a| a.attempt == attempt) else {
                return;
            };
            attempt_entry.result = Some(HealthResult { success, exit_code });
        }
        self.inner.publish(id, ChangeKind::Health);
    }
}

/// Build the model with explicit per-service log retention. This is the only way to mint a
/// [`SessionModelWriter`].
pub(crate) fn new_with_retention(
    initial: impl IntoIterator<Item = (ServiceSnapshot, LogRetention)>,
    spool_dir: Option<PathBuf>,
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
        disk,
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
    new_with_retention(initial, create_spool_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use similar_asserts::assert_eq;

    fn snapshot(id: &str) -> ServiceSnapshot {
        ServiceSnapshot::initial(
            id.to_string(),
            id.to_string(),
            Vec::new(),
            false,
            RestartPolicy::Never,
        )
    }

    fn entry(id: &str) -> (ServiceSnapshot, LogRetention) {
        (snapshot(id), LogRetention::default())
    }

    fn unique_spool_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("micromux-{prefix}-{nanos}"))
    }

    #[test]
    fn append_assigns_monotonic_sequence_numbers() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();
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
        assert_eq!(lines.first().map(|l| l.seq), Some(0));
        assert_eq!(lines.get(1).map(|l| l.seq), Some(1));
        assert_eq!(lines.get(1).map(|l| l.run_generation), Some(1));
        assert_eq!(lines.get(1).map(|l| l.line.clone()), Some("b".to_string()));
    }

    #[test]
    fn stderr_lines_are_prefixed_like_the_model() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();
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
    fn live_snapshot_replaces_same_id_and_appends_new_id() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();
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
    fn tail_limits_returned_lines() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();
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
    fn retained_run_ring_evicts_oldest_runs() {
        let retention = LogRetention {
            disk: DiskLogRetention { retained_runs: 2 },
            ..LogRetention::default()
        };
        let (reader, writer) = new_with_retention(
            [(snapshot("svc"), retention)],
            Some(unique_spool_dir("retained-ring")),
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

        let (reader, writer) =
            new_with_retention([entry("svc")], Some(unique_spool_dir("follow-paging")));
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
        let (reader, writer) = new_with_retention([entry("svc")], None);
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
        let (reader, writer) =
            new_with_retention([entry("svc")], Some(unique_spool_dir("empty-started-run")));
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
            new_with_retention([entry("svc")], Some(unique_spool_dir("disk-cursor")));
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
            .run_log_after(&id, 1, Some(1), Some(2))
            .expect("run retained")
            .lines
            .into_iter()
            .map(|line| line.line)
            .collect();
        assert_eq!(page, vec!["line 2".to_string(), "line 3".to_string()]);
    }

    #[test]
    fn retained_run_cursor_read_reuses_scanned_offsets() {
        let (reader, writer) =
            new_with_retention([entry("svc")], Some(unique_spool_dir("disk-cursor-index")));
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

        let first_page = reader.run_log_after(&id, 1, Some(1), Some(2));
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

        let second_page = reader.run_log_after(&id, 1, Some(3), Some(1));
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
    fn retained_run_cursor_below_offset_cache_window_does_not_skip_lines() {
        let (reader, writer) =
            new_with_retention([entry("svc")], Some(unique_spool_dir("disk-cursor-low")));
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

        let page = reader.run_log_after(&id, 1, Some(10), Some(3));
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
        let (reader, writer) = new_with_retention([entry("svc")], Some(spool.clone()));
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

    #[test]
    fn healthcheck_history_is_bounded_and_records_results() {
        let (reader, writer) = new([entry("svc")]);
        let id = "svc".to_string();
        for attempt in 1..=(HEALTH_HISTORY as u64 + 2) {
            writer.start_health_attempt(&id, attempt, format!("probe {attempt}"));
            writer.finish_health_attempt(&id, attempt, true, 0);
        }
        let history = reader.healthchecks(&id);
        assert_eq!(history.len(), HEALTH_HISTORY);
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

        writer.begin_run(&id, 1);
        writer.start_health_attempt(&id, 1, "probe old".to_string());
        writer.finish_health_attempt(&id, 1, false, 1);
        assert!(reader.latest_health(&id).is_some());

        writer.begin_run(&id, 2);
        assert!(reader.latest_health(&id).is_none());
        assert!(reader.healthchecks(&id).is_empty());
    }
}
