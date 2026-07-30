use std::collections::VecDeque;

use super::{LogLine, MemoryLogRetention};

/// Trim a string to its last `max_bytes`, respecting UTF-8 character boundaries.
#[must_use]
pub fn trim_to_last_bytes(line: String, max_bytes: usize) -> String {
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

/// Trim a string to its first `max_bytes`, respecting UTF-8 character boundaries.
#[must_use]
pub(crate) fn truncate_to_first_bytes(mut line: String, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line;
    }
    let end = line
        .char_indices()
        .map(|(idx, _)| idx)
        .take_while(|idx| *idx <= max_bytes)
        .last()
        .unwrap_or(0);
    line.truncate(end);
    line
}
pub(super) struct MemoryLogBuffer {
    pub(super) entries: VecDeque<LogLine>,
    pub(super) current_bytes: usize,
    pub(super) retention: MemoryLogRetention,
}

impl MemoryLogBuffer {
    pub(super) fn new(retention: MemoryLogRetention) -> Self {
        Self {
            entries: VecDeque::new(),
            current_bytes: 0,
            retention,
        }
    }

    pub(super) fn push(&mut self, mut line: LogLine) {
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

    pub(super) fn append(&mut self, line: LogLine) {
        self.push(line);
    }

    pub(super) fn replace_last(&mut self, line: LogLine) {
        if self.entries.back().is_some_and(|last| last.seq == line.seq) {
            if let Some(old) = self.entries.pop_back() {
                self.current_bytes = self.current_bytes.saturating_sub(old.line.len());
            }
            self.push(line);
        }
    }

    pub(super) fn reconfigure(&mut self, retention: MemoryLogRetention) {
        let entries = std::mem::take(&mut self.entries);
        self.current_bytes = 0;
        self.retention = retention;
        for line in entries {
            self.push(line);
        }
    }

    pub(super) fn clear_before_next_seq(&mut self, next_seq: u64) {
        while self.entries.front().is_some_and(|line| line.seq < next_seq) {
            let Some(old) = self.entries.pop_front() else {
                break;
            };
            self.current_bytes = self.current_bytes.saturating_sub(old.line.len());
        }
    }

    pub(super) fn lines(&self, tail: Option<usize>) -> Vec<LogLine> {
        let len = self.entries.len();
        let skip = tail.map_or(0, |n| len.saturating_sub(n));
        self.entries.iter().skip(skip).cloned().collect()
    }

    pub(super) fn lines_since(&self, after: u64) -> (Option<u64>, Vec<LogLine>) {
        let first_retained_seq = self.first_retained_seq();
        let start = self.entries.partition_point(|line| line.seq <= after);
        (
            first_retained_seq,
            self.entries.iter().skip(start).cloned().collect(),
        )
    }

    pub(super) fn first_retained_seq(&self) -> Option<u64> {
        self.entries.front().map(|line| line.seq)
    }
}

#[cfg(test)]
mod tests {
    use super::super::LogLimit;
    use super::*;
    use similar_asserts::assert_eq;

    fn log_line(seq: u64, line: &str) -> LogLine {
        LogLine {
            seq,
            run_generation: 1,
            timestamp_unix_ms: 0,
            line: line.to_string(),
        }
    }

    #[test]
    fn replace_last_ignores_a_target_that_was_evicted() {
        let mut buffer = MemoryLogBuffer::new(MemoryLogRetention {
            max_lines: LogLimit::Unbounded,
            max_bytes: LogLimit::Unbounded,
        });
        buffer.append(log_line(1, "frame one"));
        buffer.entries.clear();
        buffer.current_bytes = 0;

        buffer.replace_last(log_line(1, "frame two"));

        assert_eq!(
            buffer
                .lines(None)
                .into_iter()
                .map(|line| line.line)
                .collect::<Vec<_>>(),
            Vec::<String>::new()
        );
    }
}
