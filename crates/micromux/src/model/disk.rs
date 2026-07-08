use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

const DISK_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);
const DISK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

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

pub(super) enum DiskLogCommand {
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
pub(super) struct DiskLogWriter {
    pub(super) tx: mpsc::Sender<DiskLogCommand>,
}

impl DiskLogWriter {
    pub(super) fn begin(&self, path: PathBuf) {
        let _ = self.tx.send(DiskLogCommand::Begin { path });
    }

    pub(super) fn write(&self, path: PathBuf, record: DiskLogRecord) {
        let _ = self.tx.send(DiskLogCommand::Write { path, record });
    }

    pub(super) fn remove(&self, path: PathBuf) {
        let _ = self.tx.send(DiskLogCommand::Remove { path });
    }
}

pub(super) struct DiskLogWorker {
    pub(super) tx: Option<mpsc::Sender<DiskLogCommand>>,
    pub(super) handle: Option<thread::JoinHandle<()>>,
    pub(super) stopped: Mutex<mpsc::Receiver<()>>,
}

impl DiskLogWorker {
    pub(super) fn spawn() -> (Self, DiskLogWriter) {
        let (tx, rx) = mpsc::channel();
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
            DiskLogWriter { tx },
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
