//! Every service's logs in one pane, interleaved in time order, each line tagged with its
//! service, so a request can be followed from one service to the next.
//!
//! Records are ordered by when micromux captured them, which one clock stamps for every service,
//! including the plain lines that carry no timestamp of their own.
//! Records captured in the same millisecond keep the sidebar's order of their services, and each
//! service's records keep their own order.
//!
//! The pane draws what each service's own pane would: every service's level threshold,
//! timestamps and hidden fields apply to its lines here too.
//! A service's live screen, which it redraws in place, is one record of several lines that moves
//! to the time of its latest redraw.
//!
//! The text is merged from the lines each service keeps for its own pane, so it holds no record
//! a service already dropped, and memory stays within the services' own retention.

use ratatui::style::{Color, Stylize as _};
use ratatui::text::Span;

use crate::log_text::{LogText, RecordKey, TaggedRecord};
use crate::state::{CachedLine, Service};

/// Widest service name a tag shows in full; a longer one is cut short, so the tags leave the
/// log text room.
const MAX_TAG_NAME_WIDTH: usize = 16;

/// The theme colors that tell services' tags apart, by their place in the sidebar.
///
/// Red, the grays, black and white stay out: red reads as an error, and the others vanish on a
/// light or a dark background.
const TAG_COLORS: [Color; 8] = [
    Color::Cyan,
    Color::Magenta,
    Color::Green,
    Color::Blue,
    Color::Yellow,
    Color::LightMagenta,
    Color::LightGreen,
    Color::LightBlue,
];

/// The log pane's text of every service's logs, rebuilt from the services' own lines.
#[derive(Debug)]
pub(crate) struct AllLogs {
    pub log: LogText,
    /// Whether a service's lines or the roster changed since the text was built.
    pub dirty: bool,
}

impl AllLogs {
    pub(crate) fn new() -> Self {
        Self {
            log: LogText::default(),
            dirty: true,
        }
    }

    /// Renders the lines of `services`, in sidebar order, into the pane's text and indexes its
    /// rows for `wrap` at `wrap_width` columns, unless that is done already.
    pub(crate) fn render_text(&mut self, services: &[Service], wrap: bool, wrap_width: u16) {
        if !self.dirty {
            self.log.relayout(wrap, wrap_width);
            return;
        }
        let name_width = services
            .iter()
            .map(|service| Span::raw(service.snapshot.id.as_str()).width())
            .max()
            .unwrap_or_default()
            .min(MAX_TAG_NAME_WIDTH);
        let tags = services
            .iter()
            .enumerate()
            .map(|(index, service)| tag(&service.snapshot.id, index, name_width))
            .collect::<Vec<_>>();
        let records = merge(services).filter_map(|merged| {
            Some(TaggedRecord {
                key: merged.key,
                text: merged.text,
                tag: tags.get(merged.service)?,
            })
        });
        self.log.rebuild_tagged(records, wrap, wrap_width);
        self.dirty = false;
    }
}

/// The spans that lead each line of the service `id` at `index` in the sidebar: its name in
/// its color, padded or cut to `name_width` columns, and a separator.
fn tag(id: &str, index: usize, name_width: usize) -> Vec<Span<'static>> {
    let color = TAG_COLORS
        .get(index % TAG_COLORS.len())
        .copied()
        .unwrap_or(Color::Cyan);
    let full_width = Span::raw(id).width();
    let mut name = String::new();
    let mut width = 0;
    if full_width <= name_width {
        name.push_str(id);
        width = full_width;
    } else if name_width > 0 {
        // A name cut short keeps a column for the ellipsis that says so.
        for ch in id.chars() {
            let ch_width = Span::raw(ch.to_string()).width();
            if width + ch_width + 1 > name_width {
                break;
            }
            name.push(ch);
            width += ch_width;
        }
        name.push('…');
        width += 1;
    }
    name.push_str(&" ".repeat(name_width.saturating_sub(width)));
    vec![name.fg(color).bold(), " │ ".fg(Color::DarkGray)]
}

/// A record of one service in the merged order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MergedRecord<'a> {
    key: RecordKey,
    /// The service's index in the sidebar.
    service: usize,
    text: &'a str,
}

/// The lines of every service in `services` merged by capture time, then by the service's place
/// in the sidebar, keeping each service's own order.
///
/// Capture times only grow within a service's log, but a key never goes below the one before it
/// in the same service even where they do not, so the keys always order the records as merged.
fn merge(services: &[Service]) -> impl Iterator<Item = MergedRecord<'_>> {
    let mut cursors = services
        .iter()
        .map(|service| ServiceCursor {
            lines: service.cached_lines.iter().peekable(),
            floor_ms: 0,
        })
        .collect::<Vec<_>>();
    std::iter::from_fn(move || {
        // A sidebar holds few services, so the next record is the least of their next ones.
        let (service, key) = cursors
            .iter_mut()
            .enumerate()
            .filter_map(|(index, cursor)| Some((index, cursor.next_key(index)?)))
            .min_by_key(|(_, key)| *key)?;
        let cursor = cursors.get_mut(service)?;
        let line = cursor.lines.next()?;
        cursor.floor_ms = key.timestamp_unix_ms;
        Some(MergedRecord {
            key,
            service,
            text: &line.text,
        })
    })
}

/// How far the merge has read one service's lines.
struct ServiceCursor<'a> {
    lines: std::iter::Peekable<std::collections::vec_deque::Iter<'a, CachedLine>>,
    /// The capture time of the service's record merged last.
    floor_ms: u64,
}

impl ServiceCursor<'_> {
    /// The key of the service's next line, for the service at `index` in the sidebar.
    fn next_key(&mut self, index: usize) -> Option<RecordKey> {
        let line = self.lines.peek()?;
        Some(RecordKey {
            timestamp_unix_ms: line.timestamp_unix_ms.max(self.floor_ms),
            service: u32::try_from(index).unwrap_or(u32::MAX),
            seq: line.seq,
        })
    }
}

#[cfg(test)]
mod tests;
