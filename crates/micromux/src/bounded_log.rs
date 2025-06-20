use std::collections::VecDeque;

/// A log buffer that retains only the most recent entries, bounded by line count and/or total bytes.
#[derive(Debug)]
pub struct BoundedLog {
    entries: VecDeque<String>,
    max_lines: Option<usize>,
    max_bytes: Option<usize>,
    current_bytes: usize,
}

impl BoundedLog {
    /// Create a new BoundedLog with optional line and byte limits.
    ///
    /// - `max_lines`: keeps at most this many lines (if `Some`).
    /// - `max_bytes`: keeps at most this many bytes total (if `Some`).
    pub fn new(max_lines: Option<usize>, max_bytes: Option<usize>) -> Self {
        BoundedLog {
            entries: VecDeque::new(),
            max_lines,
            max_bytes,
            current_bytes: 0,
        }
    }

    /// Keep only the most recent `max_lines` lines.
    pub fn with_max_lines(max_lines: usize) -> Self {
        Self::new(Some(max_lines), None)
    }

    /// Keep only the most recent content fitting in `max_bytes` bytes.
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self::new(None, Some(max_bytes))
    }

    /// Keep at most `max_lines` and at most `max_bytes`.
    pub fn with_limits(max_lines: usize, max_bytes: usize) -> Self {
        Self::new(Some(max_lines), Some(max_bytes))
    }

    /// Push a new log line into the buffer, evicting old entries as needed.
    pub fn push(&mut self, line: String) {
        let line_len = line.len();

        // Enforce byte limit first (evict from front until under the limit)
        if let Some(max_bytes) = self.max_bytes {
            while self.current_bytes + line_len > max_bytes {
                if let Some(old) = self.entries.pop_front() {
                    self.current_bytes = self.current_bytes.saturating_sub(old.len());
                } else {
                    break;
                }
            }
        }

        // Add the new line
        self.entries.push_back(line);
        self.current_bytes += line_len;

        // Enforce line count limit
        if let Some(max_lines) = self.max_lines {
            while self.entries.len() > max_lines {
                if let Some(old) = self.entries.pop_front() {
                    self.current_bytes = self.current_bytes.saturating_sub(old.len());
                }
            }
        }
    }

    /// Iterate over the retained log lines, in order (oldest first).
    pub fn entries(&self) -> impl Iterator<Item = &String> {
        self.entries.iter()
    }

    /// Clears all entries from the log.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.current_bytes = 0;
    }
}
