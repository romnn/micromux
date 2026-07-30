use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read as _, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

use crate::scheduler::ServiceID;

use super::disk::{
    DiskLogOp, DiskLogRecord, DiskLogWriter, DiskRunMetadata, SharedDiskRunMetadata,
};
use super::{LogLine, LogRunSummary};

pub(super) const RUN_LOG_OFFSET_CACHE: usize = 4096;
const MAX_STABLE_SEGMENT_READ_ATTEMPTS: usize = 3;

#[derive(Clone, Debug, Default)]
pub(super) struct RunLogReadIndex {
    pub(super) segment_epoch: u64,
    pub(super) scanned_to: u64,
    pub(super) last_append_seq: Option<u64>,
    pub(super) append_offsets: VecDeque<(u64, u64)>,
}

impl RunLogReadIndex {
    fn reset_scan(&mut self) {
        self.scanned_to = 0;
        self.last_append_seq = None;
        self.append_offsets.clear();
    }

    fn reset_for_segment(&mut self, segment_epoch: u64) {
        if self.segment_epoch != segment_epoch {
            self.reset_scan();
            self.segment_epoch = segment_epoch;
        }
    }

    fn reset_if_past_end(&mut self, file_len: u64) {
        if self.scanned_to > file_len {
            self.reset_scan();
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
        let idx = self
            .append_offsets
            .partition_point(|(known_seq, _offset)| *known_seq < seq);
        if self
            .append_offsets
            .get(idx)
            .is_none_or(|(known_seq, _offset)| *known_seq != seq)
        {
            self.append_offsets.insert(idx, (seq, offset));
        }
        while self.append_offsets.len() > RUN_LOG_OFFSET_CACHE {
            self.append_offsets.pop_front();
        }
    }

    fn mark_scanned_to(&mut self, offset: u64) {
        self.scanned_to = self.scanned_to.max(offset);
    }
}

pub(super) struct RunLogEntry {
    pub(super) run_generation: u64,
    pub(super) path: Option<PathBuf>,
    pub(super) last_seq: Option<u64>,
    pub(super) live_snapshot_id: Option<u64>,
    pub(super) read_index: std::sync::Arc<Mutex<RunLogReadIndex>>,
    pub(super) disk_metadata: SharedDiskRunMetadata,
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
        let disk_metadata = std::sync::Arc::new(Mutex::new(DiskRunMetadata::default()));
        if let (Some(path), Some(disk)) = (&path, disk) {
            disk.begin(path.clone(), disk_metadata.clone());
        }

        Self {
            run_generation,
            path,
            last_seq: None,
            live_snapshot_id: None,
            read_index: std::sync::Arc::new(Mutex::new(RunLogReadIndex::default())),
            disk_metadata,
        }
    }

    pub(super) fn summary(&self, current: bool) -> LogRunSummary {
        let metadata = self.disk_metadata.lock();
        LogRunSummary {
            run_generation: self.run_generation,
            current,
            path: self.path.as_ref().map(|path| path.display().to_string()),
            line_count: metadata.line_count,
            first_seq: metadata.first_seq,
            last_seq: metadata.last_seq,
        }
    }

    pub(super) fn append_metadata(&mut self, seq: u64) {
        self.last_seq = Some(seq);
    }

    pub(super) fn replace_metadata(&mut self, seq: u64) {
        if self.last_seq.is_none() {
            self.append_metadata(seq);
        } else {
            self.last_seq = Some(seq);
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
    index: &mut RunLogReadIndex,
    disk_metadata: &SharedDiskRunMetadata,
) -> Option<Vec<LogLine>> {
    read_consistent_segment(path, index, disk_metadata, |index, file, readable_len| {
        read_run_log_segment(path, file, readable_len, tail, after, limit, index)
    })
}

fn read_consistent_segment<T>(
    path: &Path,
    index: &mut RunLogReadIndex,
    disk_metadata: &SharedDiskRunMetadata,
    mut read: impl FnMut(&mut RunLogReadIndex, File, u64) -> Option<T>,
) -> Option<T> {
    for _ in 0..MAX_STABLE_SEGMENT_READ_ATTEMPTS {
        let (segment_epoch, file, readable_len) = {
            let metadata = disk_metadata.lock();
            let file = File::open(path).ok()?;
            let readable_len = file.metadata().ok()?.len();
            (metadata.segment_epoch, file, readable_len)
        };
        let mut candidate_index = index.clone();
        candidate_index.reset_for_segment(segment_epoch);
        let value = read(&mut candidate_index, file, readable_len)?;

        // The writer holds `disk_metadata` across `write_all`, so `readable_len` cannot land inside
        // a concurrently successful record. Read only that prefix, then reject it if an in-place
        // rotation changed its epoch. This keeps long reads off the writer lock without publishing
        // a mixed segment or partial append.
        if disk_metadata.lock().segment_epoch == segment_epoch {
            *index = candidate_index;
            return Some(value);
        }
    }
    None
}

fn read_run_log_segment(
    path: &Path,
    mut file: File,
    readable_len: u64,
    tail: Option<usize>,
    after: Option<u64>,
    limit: Option<usize>,
    index: &mut RunLogReadIndex,
) -> Option<Vec<LogLine>> {
    index.reset_if_past_end(readable_len);
    let start_offset = start_offset_for_read(index, tail, after, readable_len);
    if start_offset == 0 {
        index.reset_scan();
    }

    if file.seek(SeekFrom::Start(start_offset)).is_err() {
        return None;
    }
    let mut reader = BufReader::new(file.take(readable_len.saturating_sub(start_offset)));

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
    Some(lines.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use similar_asserts::assert_eq;
    use std::fs::{self, File, OpenOptions};
    use std::io::Write as _;

    #[test]
    fn run_log_read_releases_metadata_lock_and_retries_rotation() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("run.jsonl");
        fs::write(&path, b"xx")?;
        let metadata = std::sync::Arc::new(Mutex::new(DiskRunMetadata::default()));
        let mut index = RunLogReadIndex::default();
        let mut attempts = 0;

        let value = read_consistent_segment(
            &path,
            &mut index,
            &metadata,
            |candidate, _file, readable_len| {
                attempts += 1;
                assert_eq!(readable_len, 2);
                assert!(metadata.try_lock().is_some());
                candidate.mark_scanned_to(u64::try_from(attempts).unwrap_or(u64::MAX));
                if attempts == 1 {
                    metadata.lock().segment_epoch = 1;
                }
                Some(attempts)
            },
        );

        assert_eq!(value, Some(2));
        assert_eq!(attempts, 2);
        assert_eq!(index.segment_epoch, 1);
        assert_eq!(index.scanned_to, 2);
        Ok(())
    }

    #[test]
    fn run_log_read_does_not_advance_into_a_concurrent_append() -> eyre::Result<()> {
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
            line: "second".to_string(),
            ..first.clone()
        };
        let mut file = File::create(&path)?;
        serde_json::to_writer(&mut file, &first)?;
        file.write_all(b"\n")?;
        file.flush()?;
        let initial_len = file.metadata()?.len();
        drop(file);

        let mut second_encoded = serde_json::to_vec(&second)?;
        second_encoded.push(b'\n');
        let split = second_encoded.len() / 2;
        assert!(serde_json::from_slice::<DiskLogRecord>(&second_encoded[..split]).is_err());

        let metadata = std::sync::Arc::new(Mutex::new(DiskRunMetadata {
            segment_epoch: 1,
            line_count: 1,
            first_seq: Some(1),
            last_seq: Some(1),
        }));
        let mut index = RunLogReadIndex::default();

        let first_page = read_consistent_segment(
            &path,
            &mut index,
            &metadata,
            |candidate, file, readable_len| {
                assert_eq!(readable_len, initial_len);
                // Recreate the writer's lock ordering while exposing half a record to the file.
                Some((|| -> eyre::Result<Vec<LogLine>> {
                    let mut metadata = metadata.lock();
                    let mut append = OpenOptions::new().append(true).open(&path)?;
                    append.write_all(&second_encoded[..split])?;
                    append.flush()?;

                    let lines = read_run_log_segment(
                        &path,
                        file,
                        readable_len,
                        None,
                        None,
                        None,
                        candidate,
                    )
                    .ok_or_else(|| eyre::eyre!("stable run-log prefix was unreadable"))?;

                    append.write_all(&second_encoded[split..])?;
                    append.flush()?;
                    metadata.line_count = 2;
                    metadata.last_seq = Some(2);
                    Ok(lines)
                })())
            },
        )
        .ok_or_else(|| eyre::eyre!("run-log snapshot was unavailable"))??;
        // The first read commits only the stable cursor; the next one sees the completed append.
        assert_eq!(
            first_page.iter().map(|line| line.seq).collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(index.scanned_to, initial_len);

        let second_page = read_run_log_file(&path, None, Some(1), None, &mut index, &metadata)
            .ok_or_else(|| eyre::eyre!("completed append was unreadable"))?;
        assert_eq!(
            second_page.iter().map(|line| line.seq).collect::<Vec<_>>(),
            vec![2]
        );
        Ok(())
    }

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

    #[test]
    fn segment_epoch_resets_an_index_after_truncate_and_regrow() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("run.jsonl");
        let metadata = std::sync::Arc::new(Mutex::new(DiskRunMetadata {
            segment_epoch: 1,
            line_count: 1,
            first_seq: Some(1),
            last_seq: Some(1),
        }));
        let first = DiskLogRecord {
            seq: 1,
            run_generation: 1,
            timestamp_unix_ms: 0,
            op: DiskLogOp::Append,
            line: "first".to_string(),
        };
        let mut file = File::create(&path)?;
        serde_json::to_writer(&mut file, &first)?;
        file.write_all(b"\n")?;
        drop(file);

        let mut index = RunLogReadIndex::default();
        let initial = read_run_log_file(&path, None, None, None, &mut index, &metadata)
            .ok_or_else(|| eyre::eyre!("initial run log was unreadable"))?;
        let old_scanned_to = index.scanned_to;
        assert_eq!(initial.len(), 1);

        let second = DiskLogRecord {
            seq: 2,
            line: "x".repeat(usize::try_from(old_scanned_to)?.saturating_add(32)),
            ..first
        };
        let mut file = File::create(&path)?;
        serde_json::to_writer(&mut file, &second)?;
        file.write_all(b"\n")?;
        drop(file);
        {
            let mut metadata = metadata.lock();
            metadata.segment_epoch = 2;
            metadata.first_seq = Some(2);
            metadata.last_seq = Some(2);
        }

        let after_rotation = read_run_log_file(&path, None, Some(1), None, &mut index, &metadata)
            .ok_or_else(|| eyre::eyre!("rotated run log was unreadable"))?;

        assert_eq!(after_rotation.first().map(|line| line.seq), Some(2));
        Ok(())
    }
}
