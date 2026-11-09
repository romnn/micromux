//! A service's lifecycle events timeline: the attached TUI's copy of it, and the events pane's
//! text.
//!
//! The session already explains each transition in the event's own `detail` line, the same line
//! the MCP `get_service_events` tool returns, so the pane adds only the time and a colored kind.

use std::collections::VecDeque;

use micromux::{EVENT_HISTORY, ServiceEvent, ServiceEventKind};
use ratatui::style::{Color, Style, Stylize as _};
use ratatui::text::{Line, Span, Text};

/// Width the kind column pads its labels to, the longest label's.
const KIND_WIDTH: usize = 11;

/// An attached TUI's copy of a service's retained lifecycle events, oldest first.
///
/// It holds at most [`EVENT_HISTORY`] events, as many as the session retains per service, and is
/// read forward from the newest event it holds, so a refresh fetches only what is new over the
/// connection.
#[derive(Debug, Default)]
pub(crate) struct Timeline {
    events: VecDeque<ServiceEvent>,
}

impl Timeline {
    /// The cursor to read events after: one before the newest event held, or `None` to read the
    /// whole retained history.
    ///
    /// Reading the newest event again lets [`Self::extend`] tell a continuation of this copy from
    /// a history the session no longer shares with it.
    pub(crate) fn read_after(&self) -> Option<u64> {
        self.events
            .back()
            .map(|newest| newest.seq.saturating_sub(1))
    }

    /// Appends events read after [`Self::read_after`], and returns whether they continue this
    /// copy.
    ///
    /// They continue it only when they start with the newest event held.
    /// They do not when the session dropped that event from its retention, or when the service
    /// was recreated under the same id and numbers its events from one again; the copy is then
    /// left unchanged for the caller to [`Self::replace`] with the whole retained history.
    pub(crate) fn extend(&mut self, page: Vec<ServiceEvent>) -> bool {
        let Some(newest) = self.events.back() else {
            self.replace(page);
            return true;
        };
        let mut page = page.into_iter();
        if page.next().as_ref() != Some(newest) {
            return false;
        }
        self.events.extend(page);
        self.trim();
        true
    }

    /// Replaces the copy with `events`, the service's retained history in sequence order.
    pub(crate) fn replace(&mut self, events: Vec<ServiceEvent>) {
        self.events = events.into();
        self.trim();
    }

    fn trim(&mut self) {
        while self.events.len() > EVENT_HISTORY {
            self.events.pop_front();
        }
    }

    /// Every event held, oldest first.
    pub(crate) fn events(&self) -> Vec<ServiceEvent> {
        self.events.iter().cloned().collect()
    }
}

/// The events pane's text for `events`: one line per event, oldest first, so the newest sits at
/// the bottom where follow-tail keeps it in view.
pub(crate) fn text(events: &[ServiceEvent]) -> Text<'static> {
    if events.is_empty() {
        return Text::from("no lifecycle events yet".fg(Color::DarkGray));
    }
    events.iter().map(event_line).collect()
}

/// The pane's text for a session that cannot report lifecycle events.
pub(crate) fn unsupported_text() -> Text<'static> {
    Text::from(
        format!(
            "this session predates lifecycle events; they need protocol {} or later",
            micromux_control::EVENTS_PROTOCOL_VERSION
        )
        .fg(Color::DarkGray),
    )
}

/// One event as `HH:MM:SS kind detail`, with the time in local time and the kind colored by what
/// the transition means for the service.
pub(crate) fn event_line(event: &ServiceEvent) -> Line<'static> {
    let (label, style) = kind_label(event);
    Line::from(vec![
        clock_time(event.at_unix_ms).fg(Color::DarkGray),
        " ".into(),
        Span::styled(format!("{label:<KIND_WIDTH$}"), style),
        " ".into(),
        event.detail.clone().into(),
    ])
}

/// Formats a Unix millisecond timestamp as a local wall-clock time, such as `13:29:09`.
pub(crate) fn clock_time(unix_ms: u64) -> String {
    i64::try_from(unix_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map_or_else(
            || "--:--:--".to_string(),
            |timestamp| {
                timestamp
                    .with_timezone(&chrono::Local)
                    .format("%H:%M:%S")
                    .to_string()
            },
        )
}

/// A short name for the event's kind and its style.
///
/// Red marks a failure, yellow a service waiting or input lost, green progress towards running,
/// blue a request or configuration change, and gray the end of a service or an unknown kind.
fn kind_label(event: &ServiceEvent) -> (&'static str, Style) {
    let red = Style::new().fg(Color::Red);
    let yellow = Style::new().fg(Color::Yellow);
    let green = Style::new().fg(Color::Green);
    let blue = Style::new().fg(Color::Blue);
    let gray = Style::new().fg(Color::DarkGray);
    match event.kind {
        ServiceEventKind::RestartRequested => ("restart", blue),
        ServiceEventKind::EnableRequested => ("enable", blue),
        ServiceEventKind::DisableRequested => ("disable", blue),
        ServiceEventKind::ConfigReloaded => ("config", blue),
        ServiceEventKind::SpawnFailed => ("spawn error", red),
        ServiceEventKind::Spawned => ("spawned", green),
        ServiceEventKind::Healthy => ("healthy", green),
        ServiceEventKind::Unhealthy => ("unhealthy", red),
        // A job's successful exit is its goal; any other exit ends a run unexpectedly.
        ServiceEventKind::Exited if event.exit_code == Some(0) => ("exited", green),
        ServiceEventKind::Exited => ("exited", red),
        ServiceEventKind::BackoffScheduled => ("backoff", yellow),
        ServiceEventKind::DependencyBlocked => ("blocked", yellow),
        ServiceEventKind::DependencyReady => ("deps ready", green),
        ServiceEventKind::DependencyFailed => ("dep failed", red),
        ServiceEventKind::InputDropped => ("input lost", yellow),
        ServiceEventKind::Created => ("created", blue),
        ServiceEventKind::Replaced => ("replaced", blue),
        ServiceEventKind::LeaseRenewed => ("renewed", blue),
        ServiceEventKind::Retired => ("retired", gray),
        ServiceEventKind::CleanupStarted => ("cleanup", blue),
        ServiceEventKind::CleanupFinished => ("cleaned up", green),
        ServiceEventKind::CleanupFailed => ("cleanup err", red),
        ServiceEventKind::Unknown => ("unknown", gray),
    }
}

#[cfg(test)]
mod tests;
