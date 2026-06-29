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

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::bounded_log::BoundedLog;
use crate::health_check::Health;
use crate::scheduler::{LogUpdateKind, OutputStream, ServiceID};
use crate::service::RestartPolicy;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// Per-service log retention in the model. Chosen to match the historical TUI buffer so that, once
/// the TUI reads from the model (M4), the rendered frames are identical.
const MODEL_LOG_MAX_LINES: u16 = 1000;
const MODEL_LOG_MAX_BYTES: usize = 64 * MIB;

/// How many recent healthcheck attempts the model retains per service.
const HEALTH_HISTORY: usize = 8;
/// Per-attempt healthcheck output retention.
const HEALTH_OUTPUT_MAX_LINES: usize = 200;

/// Capacity of the liveness-only change broadcast. A lagging subscriber loses only coalescible
/// notifications (it re-queries the model for content), never log bytes.
const CHANGE_CHANNEL_CAPACITY: usize = 1024;

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

/// A single retained log line with a monotonic cursor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    /// Monotonic sequence number; survives eviction so a follower can resume without gaps or dupes.
    pub seq: u64,
    /// The already-formatted line (stderr lines carry a `[stderr]` prefix, matching the TUI).
    pub line: String,
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

struct LogBuffer {
    log: BoundedLog,
    live_snapshot_id: Option<u64>,
    /// Sequence number of the oldest retained line; grows as lines are evicted.
    first_seq: u64,
}

impl LogBuffer {
    fn new() -> Self {
        Self {
            log: BoundedLog::with_limits(MODEL_LOG_MAX_LINES, MODEL_LOG_MAX_BYTES),
            live_snapshot_id: None,
            first_seq: 0,
        }
    }

    fn append(&mut self, line: String) {
        let before = self.log.len();
        self.log.push(line);
        let after = self.log.len();
        // We added one line and ended with `after`; the difference is what was evicted from the
        // front, which advances the oldest retained sequence number.
        let evicted = (before + 1).saturating_sub(after);
        self.first_seq = self.first_seq.saturating_add(evicted as u64);
        self.live_snapshot_id = None;
    }

    fn replace_last(&mut self, line: String) {
        let before = self.log.len();
        self.log.replace_last(line);
        let after = self.log.len();
        let evicted = before.saturating_sub(after);
        self.first_seq = self.first_seq.saturating_add(evicted as u64);
        self.live_snapshot_id = None;
    }

    /// Reproduces the TUI reducer's `LiveSnapshot` semantics: replace the previous frame when the
    /// id matches, otherwise append a new line.
    fn live_snapshot(&mut self, id: u64, line: String) {
        if self.live_snapshot_id == Some(id) {
            // replace_last clears live_snapshot_id, so re-set it afterwards.
            self.replace_last(line);
        } else {
            self.append(line);
        }
        self.live_snapshot_id = Some(id);
    }

    fn clear(&mut self) {
        // Keep `first_seq` monotonic across a clear so a follower never sees a sequence go backwards.
        self.first_seq = self.first_seq.saturating_add(self.log.len() as u64);
        self.log.clear();
        self.live_snapshot_id = None;
    }

    fn lines(&self, tail: Option<usize>) -> Vec<LogLine> {
        let len = self.log.len();
        let skip = match tail {
            Some(n) => len.saturating_sub(n),
            None => 0,
        };
        self.log
            .entries()
            .enumerate()
            .skip(skip)
            .map(|(idx, line)| LogLine {
                seq: self.first_seq.saturating_add(idx as u64),
                line: line.clone(),
            })
            .collect()
    }
}

struct ServiceEntry {
    snapshot: ServiceSnapshot,
    started_at: Option<Instant>,
    logs: LogBuffer,
    health: VecDeque<HealthAttempt>,
}

impl ServiceEntry {
    fn new(snapshot: ServiceSnapshot) -> Self {
        Self {
            snapshot,
            started_at: None,
            logs: LogBuffer::new(),
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
}

struct Inner {
    services: RwLock<IndexMap<ServiceID, ServiceEntry>>,
    change_tx: broadcast::Sender<SessionChange>,
}

impl Inner {
    fn publish(&self, service_id: &ServiceID, kind: ChangeKind) {
        // A send error only means there are no subscribers right now; that is fine.
        let _ = self.change_tx.send(SessionChange {
            service_id: service_id.clone(),
            kind,
        });
    }
}

/// Read capability over the session model. `Clone`, handed to every adapter (TUI, control server,
/// MCP). It has no write methods — that absence is the boundary.
#[derive(Clone)]
pub struct SessionModelReader {
    inner: Arc<Inner>,
}

impl SessionModelReader {
    /// A snapshot of every service, in configuration order, with `uptime` refreshed to now.
    #[must_use]
    pub fn services(&self) -> Vec<ServiceSnapshot> {
        let guard = self.inner.services.read();
        guard.values().map(ServiceEntry::current_snapshot).collect()
    }

    /// The snapshot of a single service, if present.
    #[must_use]
    pub fn service(&self, id: &ServiceID) -> Option<ServiceSnapshot> {
        let guard = self.inner.services.read();
        guard.get(id).map(ServiceEntry::current_snapshot)
    }

    /// The retained log lines for a service. `tail` bounds the result to the most recent lines.
    #[must_use]
    pub fn logs(&self, id: &ServiceID, tail: Option<usize>) -> Vec<LogLine> {
        let guard = self.inner.services.read();
        guard
            .get(id)
            .map(|entry| entry.logs.lines(tail))
            .unwrap_or_default()
    }

    /// The full retained healthcheck history for a service (oldest first).
    #[must_use]
    pub fn healthchecks(&self, id: &ServiceID) -> Vec<HealthAttempt> {
        let guard = self.inner.services.read();
        guard
            .get(id)
            .map(|entry| entry.health.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The most recent healthcheck attempt for a service, if any.
    #[must_use]
    pub fn latest_health(&self, id: &ServiceID) -> Option<HealthAttempt> {
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
                let mut entry = ServiceEntry::new(snapshot);
                entry.started_at = started_at;
                guard.insert(service_id.clone(), entry);
            }
        }
        self.inner.publish(&service_id, ChangeKind::Status);
    }

    /// Append a log line, reproducing the TUI reducer's `LogUpdateKind` semantics exactly.
    pub(crate) fn append_log(
        &self,
        id: &ServiceID,
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
            match update {
                LogUpdateKind::Append => entry.logs.append(line),
                LogUpdateKind::ReplaceLast => entry.logs.replace_last(line),
                LogUpdateKind::LiveSnapshot { id: snapshot_id } => {
                    entry.logs.live_snapshot(snapshot_id, line);
                }
            }
        }
        self.inner.publish(id, ChangeKind::Logs);
    }

    /// Clear the log buffer for a service (e.g. on restart).
    pub(crate) fn clear_logs(&self, id: &ServiceID) {
        {
            let mut guard = self.inner.services.write();
            let Some(entry) = guard.get_mut(id) else {
                return;
            };
            entry.logs.clear();
        }
        self.inner.publish(id, ChangeKind::Logs);
    }

    /// Reset the live-snapshot target so a new run's first frame appends rather than replacing the
    /// previous run's final frame (mirrors the reducer's handling of `Started`).
    pub(crate) fn note_run_started(&self, id: &ServiceID) {
        let mut guard = self.inner.services.write();
        if let Some(entry) = guard.get_mut(id) {
            entry.logs.live_snapshot_id = None;
        }
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

/// Build the model seeded with `initial` snapshots and return the paired handles. The writer is
/// moved into the scheduler future by `start_with_handles`; the reader is cloned to adapters. This
/// is the only constructor that can mint a [`SessionModelWriter`].
pub(crate) fn new(
    initial: impl IntoIterator<Item = ServiceSnapshot>,
) -> (SessionModelReader, SessionModelWriter) {
    let mut services = IndexMap::new();
    for snapshot in initial {
        services.insert(snapshot.id.clone(), ServiceEntry::new(snapshot));
    }
    let (change_tx, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
    let inner = Arc::new(Inner {
        services: RwLock::new(services),
        change_tx,
    });
    (
        SessionModelReader {
            inner: Arc::clone(&inner),
        },
        SessionModelWriter { inner },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use similar_asserts::assert_eq;

    fn snapshot(id: &str) -> ServiceSnapshot {
        ServiceSnapshot {
            id: id.to_string(),
            name: id.to_string(),
            desired: Desired::Enabled,
            execution: Execution::Pending,
            health: None,
            run_generation: 0,
            open_ports: Vec::new(),
            healthcheck_configured: false,
            last_exit_code: None,
            uptime: None,
            restart_policy: RestartPolicy::Never,
        }
    }

    #[test]
    fn append_assigns_monotonic_sequence_numbers() {
        let (reader, writer) = new([snapshot("svc")]);
        let id = "svc".to_string();
        writer.append_log(&id, OutputStream::Stdout, LogUpdateKind::Append, "a".into());
        writer.append_log(&id, OutputStream::Stdout, LogUpdateKind::Append, "b".into());

        let lines = reader.logs(&id, None);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines.first().map(|l| l.seq), Some(0));
        assert_eq!(lines.get(1).map(|l| l.seq), Some(1));
        assert_eq!(lines.get(1).map(|l| l.line.clone()), Some("b".to_string()));
    }

    #[test]
    fn stderr_lines_are_prefixed_like_the_reducer() {
        let (reader, writer) = new([snapshot("svc")]);
        let id = "svc".to_string();
        writer.append_log(
            &id,
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
        let (reader, writer) = new([snapshot("svc")]);
        let id = "svc".to_string();
        writer.append_log(
            &id,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 7 },
            "frame one".into(),
        );
        writer.append_log(
            &id,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 7 },
            "frame two".into(),
        );
        writer.append_log(
            &id,
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
        let (reader, writer) = new([snapshot("svc")]);
        let id = "svc".to_string();
        for i in 0..5 {
            writer.append_log(
                &id,
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

    #[tokio::test]
    async fn subscribe_observes_a_status_change() {
        let (reader, writer) = new([snapshot("svc")]);
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
        let (reader, writer) = new([snapshot("svc")]);
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
}
