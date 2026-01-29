use itertools::Itertools;
use std::collections::VecDeque;
use std::sync::Arc;
use parking_lot::RwLock;
use tokio::sync::watch;

/// A log buffer that retains only the most recent entries, bounded by line count and/or total bytes.
#[derive(Debug)]
pub struct BoundedLog {
    entries: VecDeque<String>,
    max_lines: u16,
    max_bytes: Option<usize>,
    current_bytes: usize,
}

impl BoundedLog {
    /// Create a new `BoundedLog` with optional line and byte limits.
    ///
    /// - `max_lines`: keeps at most this many lines (if `Some`).
    /// - `max_bytes`: keeps at most this many bytes total (if `Some`).
    #[must_use]
    pub fn new(max_lines: Option<u16>, max_bytes: Option<usize>) -> Self {
        BoundedLog {
            entries: VecDeque::new(),
            max_lines: max_lines.unwrap_or(u16::MAX),
            max_bytes,
            current_bytes: 0,
        }
    }

    /// Keep only the most recent `max_lines` lines.
    #[must_use]
    pub fn with_max_lines(max_lines: u16) -> Self {
        Self::new(Some(max_lines), None)
    }

    /// Keep only the most recent content fitting in `max_bytes` bytes.
    #[must_use]
    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self::new(None, Some(max_bytes))
    }

    /// Keep at most `max_lines` and at most `max_bytes`.
    #[must_use]
    pub fn with_limits(max_lines: u16, max_bytes: usize) -> Self {
        Self::new(Some(max_lines), Some(max_bytes))
    }

    /// Number of log lines in the buffer.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if the buffer contains no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
        while self.entries.len() > self.max_lines.into() {
            if let Some(old) = self.entries.pop_front() {
                self.current_bytes = self.current_bytes.saturating_sub(old.len());
            }
        }
    }

    /// Replace the last line in the buffer.
    ///
    /// If the buffer is empty, this behaves like [`push`].
    pub fn replace_last(&mut self, line: String) {
        if let Some(old) = self.entries.pop_back() {
            self.current_bytes = self.current_bytes.saturating_sub(old.len());
        }
        self.push(line);
    }

    /// Iterate over the retained log lines, in order (oldest first).
    pub fn entries(&self) -> impl Iterator<Item = &String> {
        self.entries.iter()
    }

    /// Return the full contents joined with `\n`.
    #[must_use]
    pub fn full_text(&self) -> String {
        self.entries.iter().join("\n")
    }

    /// Clears all entries from the log.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.current_bytes = 0;
    }
}

/// An async wrapper around `BoundedLog` that supports subscriptions.
#[derive(Debug, Clone)]
pub struct AsyncBoundedLog {
    inner: Arc<RwLock<BoundedLog>>,
    tx: watch::Sender<u64>,
}

impl From<BoundedLog> for AsyncBoundedLog {
    fn from(log: BoundedLog) -> Self {
        Self::new(log)
    }
}

impl AsyncBoundedLog {
    /// Create with optional limits.
    #[must_use]
    pub fn new(log: BoundedLog) -> Self {
        let (tx, _) = watch::channel(0);
        AsyncBoundedLog {
            inner: Arc::new(RwLock::new(log)),
            tx,
        }
    }

    /// Push a line and notify subscribers.
    pub fn push(&self, line: String) {
        {
            let mut log = self.inner.write();
            log.push(line);
        }
        // bump version to signal update
        let ver = self.tx.borrow().wrapping_add(1);
        let _ = self.tx.send(ver);
    }

    /// Replace the last line in the buffer and notify subscribers.
    ///
    /// If the buffer is empty, this behaves like [`push`].
    pub fn replace_last(&self, line: String) {
        {
            let mut log = self.inner.write();
            log.replace_last(line);
        }
        // bump version to signal update
        let ver = self.tx.borrow().wrapping_add(1);
        let _ = self.tx.send(ver);
    }

    /// Return the number of lines and full text joined with `\n`.
    #[must_use]
    pub fn full_text(&self) -> (u16, String) {
        let log = self.inner.read();
        let lines = u16::try_from(log.len()).unwrap_or(u16::MAX);
        (lines, log.full_text())
    }

    /// Subscribe to updates; resolves when a new line is pushed.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.tx.subscribe()
    }
}
