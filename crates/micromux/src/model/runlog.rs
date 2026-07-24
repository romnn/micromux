use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::scheduler::ServiceID;

use super::disk::{DiskLogOp, DiskLogRecord, DiskLogWriter};
use super::{LogLine, LogRunSummary};

pub(super) const RUN_LOG_OFFSET_CACHE: usize = 4096;

#[derive(Clone, Debug, Default)]
pub(super) struct RunLogReadIndex {
    pub(super) scanned_to: u64,
    pub(super) last_append_seq: Option<u64>,
    pub(super) append_offsets: Vec<(u64, u64)>,
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

pub(super) struct RunLogEntry {
    pub(super) run_generation: u64,
    pub(super) path: Option<PathBuf>,
    pub(super) line_count: usize,
    pub(super) first_seq: Option<u64>,
    pub(super) last_seq: Option<u64>,
    pub(super) live_snapshot_id: Option<u64>,
    pub(super) read_index: RunLogReadIndex,
}

impl RunLogEntry {
    pub(super) fn new(
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

    pub(super) fn summary(&self, current: bool) -> LogRunSummary {
        LogRunSummary {
            run_generation: self.run_generation,
            current,
            path: self.path.as_ref().map(|path| path.display().to_string()),
            line_count: self.line_count,
            first_seq: self.first_seq,
            last_seq: self.last_seq,
        }
    }

    pub(super) fn append_metadata(&mut self, seq: u64) {
        self.line_count = self.line_count.saturating_add(1);
        self.first_seq.get_or_insert(seq);
        self.last_seq = Some(seq);
    }

    pub(super) fn replace_metadata(&mut self, seq: u64) {
        if self.line_count == 0 {
            self.append_metadata(seq);
        } else {
            self.last_seq = Some(seq);
        }
    }

    pub(super) fn enqueue_write(&self, disk: Option<&DiskLogWriter>, record: DiskLogRecord) {
        if let (Some(path), Some(disk)) = (&self.path, disk) {
            disk.write(path.clone(), record);
        }
    }

    pub(super) fn enqueue_remove(&mut self, disk: Option<&DiskLogWriter>) {
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

pub(super) fn create_spool_dir() -> Option<(PathBuf, File)> {
    let Some(base) = crate::project_dir().map(|dirs| dirs.cache_dir().join("session-run-logs"))
    else {
        tracing::warn!("disk run logs disabled: no project cache directory is available");
        return None;
    };
    if let Err(err) = fs::create_dir_all(&base) {
        tracing::warn!(?err, path = %base.display(), "disk run logs disabled");
        return None;
    }
    gc_stale_spool_dirs(&base);
    let prefix = format!("{}-", std::process::id());
    match create_spool_dir_in(&base, &prefix) {
        Ok(path) => Some(path),
        Err(err) => {
            tracing::warn!(?err, path = %base.display(), "disk run logs disabled");
            None
        }
    }
}

pub(super) fn create_spool_dir_in(base: &Path, prefix: &str) -> std::io::Result<(PathBuf, File)> {
    let mut builder = tempfile::Builder::new();
    builder.prefix(prefix);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let path = builder.tempdir_in(base).map(tempfile::TempDir::keep)?;
    let lock_path = path.join("owner.lock");
    let lock = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&lock_path)?;
    if let Err(err) = fs2::FileExt::try_lock_exclusive(&lock) {
        let _ = fs::remove_dir_all(&path);
        return Err(err);
    }
    Ok((path, lock))
}

pub(super) fn gc_stale_spool_dirs(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let lock_path = dir.join("owner.lock");
        let reclaim = match OpenOptions::new().read(true).write(true).open(&lock_path) {
            Ok(lock) => fs2::FileExt::try_lock_exclusive(&lock).is_ok(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => entry
                .metadata()
                .and_then(|meta| meta.modified())
                .map(|modified| {
                    modified
                        .elapsed()
                        .is_ok_and(|age| age > Duration::from_hours(1))
                })
                .unwrap_or(false),
            Err(_) => false,
        };
        if reclaim && let Err(err) = fs::remove_dir_all(&dir) {
            tracing::debug!(?err, path = %dir.display(), "failed to remove stale log spool");
        }
    }
}

pub(super) fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
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
    if let Some(tail) = tail {
        let Some(last) = index.last_append_seq else {
            return 0;
        };
        let tail = u64::try_from(tail).unwrap_or(u64::MAX);
        let target = last.saturating_sub(tail);
        return index.offset_at_or_before(target).unwrap_or(0).min(file_len);
    }
    let Some(after) = after else {
        return 0;
    };
    if index.last_append_seq.is_some_and(|seq| seq <= after) {
        return index.scanned_to.min(file_len);
    }
    index.offset_at_or_before(after).unwrap_or(0)
}

pub(super) fn read_run_log_file(
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
            timestamp_unix_ms: record.timestamp_unix_ms,
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

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    #[cfg(unix)]
    use similar_asserts::assert_eq;
    use std::fs::{self, File, OpenOptions};

    #[cfg(unix)]
    #[test]
    fn created_spool_dir_is_owner_private() -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let base = tempfile::tempdir()?;
        let (path, lock) = create_spool_dir_in(base.path(), "micromux-test-")?;

        let mode = fs::metadata(&path)?.permissions().mode() & !0o170_000;
        assert_eq!(mode, 0o700);
        drop(lock);
        fs::remove_dir_all(path)?;
        Ok(())
    }

    #[test]
    fn stale_spool_gc_removes_only_unlocked_owned_dirs() -> eyre::Result<()> {
        use fs2::FileExt as _;

        let base = tempfile::tempdir()?;
        let stale = base.path().join("stale");
        fs::create_dir(&stale)?;
        File::create(stale.join("owner.lock"))?;

        let locked = base.path().join("locked");
        fs::create_dir(&locked)?;
        let locked_owner = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(locked.join("owner.lock"))?;
        locked_owner.try_lock_exclusive()?;

        let fresh_without_lock = base.path().join("fresh-without-lock");
        fs::create_dir(&fresh_without_lock)?;

        gc_stale_spool_dirs(base.path());

        assert!(!stale.exists());
        assert!(locked.exists());
        assert!(fresh_without_lock.exists());
        Ok(())
    }
}
