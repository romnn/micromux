use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

const DISK_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
const DISK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const DISK_LOG_QUEUE_MAX_BYTES: usize = 8 * 1024 * 1024;
const DISK_RUN_FILE_MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(super) enum DiskLogOp {
    Append,
    ReplaceLast,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct DiskLogRecord {
    pub(super) seq: u64,
    pub(super) run_generation: u64,
    #[serde(default)]
    pub(super) timestamp_unix_ms: u64,
    pub(super) op: DiskLogOp,
    pub(super) line: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct DiskRunMetadata {
    pub(super) segment_epoch: u64,
    pub(super) line_count: usize,
    pub(super) first_seq: Option<u64>,
    pub(super) last_seq: Option<u64>,
}

impl DiskRunMetadata {
    fn begin_segment(&mut self) {
        self.segment_epoch = self.segment_epoch.saturating_add(1);
        self.line_count = 0;
        self.first_seq = None;
        self.last_seq = None;
    }

    fn observe(&mut self, op: DiskLogOp, seq: u64) {
        match op {
            DiskLogOp::Append => {
                self.line_count = self.line_count.saturating_add(1);
                self.first_seq.get_or_insert(seq);
                self.last_seq = Some(seq);
            }
            DiskLogOp::ReplaceLast => {
                self.last_seq = Some(seq);
            }
        }
    }
}

pub(super) type SharedDiskRunMetadata = Arc<Mutex<DiskRunMetadata>>;

enum DiskLogCommand {
    Begin {
        path: PathBuf,
        metadata: SharedDiskRunMetadata,
    },
    Write {
        path: PathBuf,
        metadata: SharedDiskRunMetadata,
        record: DiskLogRecord,
        _permit: DiskQueuePermit,
    },
    Remove {
        path: PathBuf,
    },
    Flush {
        done: mpsc::Sender<()>,
    },
}

#[derive(Clone)]
pub(super) struct DiskLogWriter {
    tx: mpsc::Sender<DiskLogCommand>,
    budget: Arc<DiskQueueBudget>,
}

impl DiskLogWriter {
    pub(super) fn begin(&self, path: PathBuf, metadata: SharedDiskRunMetadata) {
        let _ = self.tx.send(DiskLogCommand::Begin { path, metadata });
    }

    pub(super) fn write(
        &self,
        path: PathBuf,
        metadata: SharedDiskRunMetadata,
        record: DiskLogRecord,
    ) {
        let queued_bytes = serde_json::to_vec(&record).map_or(usize::MAX, |encoded| {
            encoded
                .len()
                .saturating_add(1)
                .saturating_add(path.as_os_str().as_encoded_bytes().len())
                .saturating_add(std::mem::size_of::<DiskLogCommand>())
        });
        let Some(permit) = self.budget.try_reserve(queued_bytes) else {
            if !self.budget.overflow_warned.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    queue_limit_bytes = DISK_LOG_QUEUE_MAX_BYTES,
                    "disk log queue is full; dropping new log records until the writer catches up"
                );
            }
            return;
        };
        let _ = self.tx.send(DiskLogCommand::Write {
            path,
            metadata,
            record,
            _permit: permit,
        });
    }

    pub(super) fn remove(&self, path: PathBuf) {
        let _ = self.tx.send(DiskLogCommand::Remove { path });
    }
}

struct DiskQueueBudget {
    queued_bytes: AtomicUsize,
    overflow_warned: AtomicBool,
}

impl DiskQueueBudget {
    fn new() -> Self {
        Self {
            queued_bytes: AtomicUsize::new(0),
            overflow_warned: AtomicBool::new(false),
        }
    }

    fn try_reserve(self: &Arc<Self>, bytes: usize) -> Option<DiskQueuePermit> {
        let mut current = self.queued_bytes.load(Ordering::Relaxed);
        loop {
            let next = current.checked_add(bytes)?;
            if next > DISK_LOG_QUEUE_MAX_BYTES {
                return None;
            }
            match self.queued_bytes.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    if current < DISK_LOG_QUEUE_MAX_BYTES / 2 {
                        self.overflow_warned.store(false, Ordering::Relaxed);
                    }
                    return Some(DiskQueuePermit {
                        budget: self.clone(),
                        bytes,
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}

struct DiskQueuePermit {
    budget: Arc<DiskQueueBudget>,
    bytes: usize,
}

impl Drop for DiskQueuePermit {
    fn drop(&mut self) {
        self.budget
            .queued_bytes
            .fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

pub(super) struct DiskLogWorker {
    tx: Option<mpsc::Sender<DiskLogCommand>>,
    handle: Option<thread::JoinHandle<()>>,
    stopped: Mutex<mpsc::Receiver<()>>,
}

impl DiskLogWorker {
    pub(super) fn spawn() -> (Self, DiskLogWriter) {
        let (tx, rx) = mpsc::channel();
        let budget = Arc::new(DiskQueueBudget::new());
        let (stopped_tx, stopped) = mpsc::channel();
        let handle = thread::spawn(move || {
            run_disk_log_worker(rx);
            let _ = stopped_tx.send(());
        });
        (
            Self {
                tx: Some(tx.clone()),
                handle: Some(handle),
                stopped: Mutex::new(stopped),
            },
            DiskLogWriter { tx, budget },
        )
    }

    pub(super) fn shutdown(&mut self) -> bool {
        self.shutdown_with_timeout(DISK_SHUTDOWN_TIMEOUT)
    }

    pub(super) fn shutdown_with_timeout(&mut self, timeout: Duration) -> bool {
        self.tx.take();
        let Some(handle) = self.handle.take() else {
            return true;
        };

        let stopped = self.stopped.get_mut().recv_timeout(timeout).is_ok() || handle.is_finished();
        if !stopped {
            tracing::warn!(
                "timed out shutting down disk log worker; leaving spool directory for safety"
            );
            return false;
        }

        if let Err(err) = handle.join() {
            tracing::debug!(?err, "disk log worker panicked during shutdown");
        }
        true
    }

    pub(super) fn flush(&self) {
        self.flush_with_timeout(DISK_FLUSH_TIMEOUT);
    }

    pub(super) fn flush_with_timeout(&self, timeout: Duration) {
        let Some(tx) = &self.tx else {
            return;
        };
        let (done, wait) = mpsc::channel();
        if tx.send(DiskLogCommand::Flush { done }).is_ok() {
            match wait.recv_timeout(timeout) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    tracing::warn!(
                        "timed out flushing disk run logs; returning last flushed content"
                    );
                }
            }
        }
    }
}

impl Drop for DiskLogWorker {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

struct DiskFileWriter {
    writer: BufWriter<File>,
    bytes_written: u64,
}

fn open_disk_log_writer(path: &Path, truncate: bool) -> Option<DiskFileWriter> {
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
        Ok(file) => {
            let bytes_written = if truncate {
                0
            } else {
                file.metadata().map_or(0, |metadata| metadata.len())
            };
            Some(DiskFileWriter {
                writer: BufWriter::new(file),
                bytes_written,
            })
        }
        Err(err) => {
            tracing::warn!(?err, path = %path.display(), "failed to open run log file");
            None
        }
    }
}

fn write_disk_record(
    writers: &mut HashMap<PathBuf, DiskFileWriter>,
    path: &Path,
    metadata: &SharedDiskRunMetadata,
    record: &DiskLogRecord,
) {
    write_disk_record_with_limit(writers, path, metadata, record, DISK_RUN_FILE_MAX_BYTES);
}

fn write_disk_record_with_limit(
    writers: &mut HashMap<PathBuf, DiskFileWriter>,
    path: &Path,
    metadata: &SharedDiskRunMetadata,
    record: &DiskLogRecord,
    max_bytes: u64,
) {
    let mut metadata = metadata.lock();
    if !writers.contains_key(path) {
        let Some(writer) = open_disk_log_writer(path, false) else {
            return;
        };
        writers.insert(path.to_path_buf(), writer);
    }

    let mut retained = record.clone();
    if matches!(retained.op, DiskLogOp::ReplaceLast) && metadata.last_seq != Some(retained.seq) {
        retained.op = DiskLogOp::Append;
    }
    let Ok(mut encoded) = serde_json::to_vec(&retained) else {
        tracing::warn!(path = %path.display(), "failed to encode run log record");
        return;
    };
    encoded.push(b'\n');
    let mut record_bytes = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
    let rotates = writers.get(path).is_some_and(|writer| {
        writer.bytes_written > 0 && writer.bytes_written.saturating_add(record_bytes) > max_bytes
    });
    if rotates {
        writers.remove(path);
        let Some(writer) = open_disk_log_writer(path, true) else {
            return;
        };
        writers.insert(path.to_path_buf(), writer);
        metadata.begin_segment();
        if matches!(retained.op, DiskLogOp::ReplaceLast) {
            // The truncated segment has no preceding record to replace, so retain the current
            // snapshot as its first append.
            retained.op = DiskLogOp::Append;
            let Ok(reencoded) = serde_json::to_vec(&retained) else {
                tracing::warn!(path = %path.display(), "failed to encode run log record");
                return;
            };
            encoded = reencoded;
            encoded.push(b'\n');
            record_bytes = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
        }
    }

    let Some(writer) = writers.get_mut(path) else {
        return;
    };
    let result = writer.writer.write_all(&encoded);
    if let Err(err) = result {
        tracing::warn!(?err, path = %path.display(), "disabling disk run log writer after write failure");
        writers.remove(path);
    } else {
        writer.bytes_written = writer.bytes_written.saturating_add(record_bytes);
        metadata.observe(retained.op, retained.seq);
    }
}

fn run_disk_log_worker(rx: mpsc::Receiver<DiskLogCommand>) {
    let mut writers = HashMap::new();
    for command in rx {
        match command {
            DiskLogCommand::Begin { path, metadata } => {
                let mut metadata = metadata.lock();
                writers.remove(&path);
                if let Some(writer) = open_disk_log_writer(&path, true) {
                    metadata.begin_segment();
                    writers.insert(path, writer);
                }
            }
            DiskLogCommand::Write {
                path,
                metadata,
                record,
                _permit: _,
            } => {
                write_disk_record(&mut writers, &path, &metadata, &record);
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
                    if let Err(err) = writer.writer.flush() {
                        tracing::warn!(?err, path = %path.display(), "failed to flush run log");
                    }
                }
                let _ = done.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use similar_asserts::assert_eq;
    use std::time::Instant;

    #[test]
    fn disk_flush_returns_when_worker_does_not_ack() {
        let (tx, _rx) = mpsc::channel();
        let (_stopped_tx, stopped) = mpsc::channel();
        let worker = DiskLogWorker {
            tx: Some(tx),
            handle: None,
            stopped: Mutex::new(stopped),
        };

        let start = Instant::now();
        worker.flush_with_timeout(Duration::from_millis(1));

        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn disk_shutdown_observes_worker_exit() {
        let (mut worker, writer) = DiskLogWorker::spawn();
        drop(writer);

        assert!(worker.shutdown_with_timeout(Duration::from_secs(1)));
    }

    #[test]
    fn disk_queue_budget_rejects_growth_past_its_byte_limit() {
        let budget = Arc::new(DiskQueueBudget::new());
        let permit = budget
            .try_reserve(DISK_LOG_QUEUE_MAX_BYTES)
            .expect("the exact queue limit should fit");

        assert!(budget.try_reserve(1).is_none());
        drop(permit);
        assert!(budget.try_reserve(1).is_some());
    }

    #[test]
    fn run_log_rotates_in_place_at_the_byte_limit() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("run.jsonl");
        let first = DiskLogRecord {
            seq: 1,
            run_generation: 1,
            timestamp_unix_ms: 0,
            op: DiskLogOp::Append,
            line: "first".to_string(),
        };
        let second = DiskLogRecord {
            seq: 2,
            run_generation: 1,
            timestamp_unix_ms: 0,
            op: DiskLogOp::Append,
            line: "second".to_string(),
        };
        let first_bytes = serde_json::to_vec(&first)?.len().saturating_add(1);
        let mut writers = HashMap::new();
        let metadata = Arc::new(Mutex::new(DiskRunMetadata::default()));

        write_disk_record_with_limit(
            &mut writers,
            &path,
            &metadata,
            &first,
            u64::try_from(first_bytes)?,
        );
        write_disk_record_with_limit(
            &mut writers,
            &path,
            &metadata,
            &second,
            u64::try_from(first_bytes)?,
        );
        if let Some(writer) = writers.get_mut(&path) {
            writer.writer.flush()?;
        }
        let contents = fs::read_to_string(path)?;

        assert!(!contents.contains("\"seq\":1"));
        assert!(contents.contains("\"seq\":2"));
        let metadata = metadata.lock();
        assert_eq!(metadata.segment_epoch, 1);
        assert_eq!(metadata.line_count, 1);
        assert_eq!(metadata.first_seq, Some(2));
        Ok(())
    }

    #[test]
    fn rotation_retains_a_replacement_as_the_new_segment_head() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("run.jsonl");
        let first = DiskLogRecord {
            seq: 1,
            run_generation: 1,
            timestamp_unix_ms: 0,
            op: DiskLogOp::Append,
            line: "first".to_string(),
        };
        let replacement = DiskLogRecord {
            op: DiskLogOp::ReplaceLast,
            line: "current frame".to_string(),
            ..first.clone()
        };
        let first_bytes = serde_json::to_vec(&first)?.len().saturating_add(1);
        let mut writers = HashMap::new();
        let metadata = Arc::new(Mutex::new(DiskRunMetadata::default()));

        write_disk_record_with_limit(
            &mut writers,
            &path,
            &metadata,
            &first,
            u64::try_from(first_bytes)?,
        );
        write_disk_record_with_limit(
            &mut writers,
            &path,
            &metadata,
            &replacement,
            u64::try_from(first_bytes)?,
        );
        if let Some(writer) = writers.get_mut(&path) {
            writer.writer.flush()?;
        }
        let contents = fs::read_to_string(path)?;
        let retained: DiskLogRecord = serde_json::from_str(contents.trim())?;

        assert!(matches!(retained.op, DiskLogOp::Append));
        assert_eq!(retained.line, "current frame");
        Ok(())
    }

    #[test]
    fn replacement_after_a_dropped_append_becomes_an_append() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("run.jsonl");
        let first = DiskLogRecord {
            seq: 1,
            run_generation: 1,
            timestamp_unix_ms: 0,
            op: DiskLogOp::Append,
            line: "first".to_string(),
        };
        let replacement = DiskLogRecord {
            seq: 2,
            op: DiskLogOp::ReplaceLast,
            line: "surviving frame".to_string(),
            ..first.clone()
        };
        let mut writers = HashMap::new();
        let metadata = Arc::new(Mutex::new(DiskRunMetadata::default()));

        write_disk_record_with_limit(&mut writers, &path, &metadata, &first, u64::MAX);
        write_disk_record_with_limit(&mut writers, &path, &metadata, &replacement, u64::MAX);
        if let Some(writer) = writers.get_mut(&path) {
            writer.writer.flush()?;
        }
        let records = fs::read_to_string(path)?
            .lines()
            .map(serde_json::from_str::<DiskLogRecord>)
            .collect::<Result<Vec<_>, _>>()?;

        assert!(matches!(
            records.get(1).map(|record| record.op),
            Some(DiskLogOp::Append)
        ));
        assert_eq!(metadata.lock().line_count, 2);
        Ok(())
    }
}
