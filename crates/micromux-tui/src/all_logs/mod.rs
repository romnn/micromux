//! Every service's logs in one pane, interleaved in time order, each line tagged with its
//! service, so a request can be followed from one service to the next.
//!
//! Records are ordered by when micromux first captured them, which one clock stamps for every
//! service, including the plain lines that carry no timestamp of their own.
//! Records captured in the same millisecond go in the order of their services' IDs, as the MCP
//! tool `follow_all_logs` orders them, and each service's records keep their own order.
//! A service's live screen, which it redraws in place, is one record of several lines that stays
//! at the time of its first frame.
//!
//! The pane draws what each service's own pane would: every service's level threshold,
//! timestamps and hidden fields apply to its lines here too.
//!
//! The text is merged from the lines each service keeps for its own pane, so memory stays within
//! the services' own retention.
//! Services keep different amounts, so the text starts at the oldest line of the service that
//! dropped lines most recently: before it, that service's lines would be missing.
//!
//! While services only log new lines and drop their oldest, the text is edited at its ends
//! rather than built again.

use std::collections::HashMap;

use ratatui::style::{Color, Stylize as _};
use ratatui::text::Span;

use crate::log_text::{LogText, RecordId, RecordKey, TaggedRecord};
use crate::state::{CachedLine, Service};

/// Widest service name a tag shows in full; a longer one is cut short, so the tags leave the
/// log text room.
const MAX_TAG_NAME_WIDTH: usize = 16;

/// The theme colors that tell services' tags apart.
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

/// The log pane's text of every service's logs, built from the services' own lines.
#[derive(Debug)]
pub(crate) struct AllLogs {
    pub log: LogText,
    /// Whether the roster changed since the text was built, which builds it again.
    pub dirty: bool,
    /// The number of each service ID, which stays with the service while the roster changes.
    numbers: HashMap<String, u32>,
    /// The service ID of each number.
    names: Vec<String>,
    /// The tag of each service, by its number.
    tags: HashMap<u32, Vec<Span<'static>>>,
    /// What the text holds of each service, by its number.
    merged: HashMap<u32, MergedService>,
    /// The last record in the text.
    last: Option<RecordKey>,
    /// The record the text starts at, when a service dropped lines.
    start: Option<RecordKey>,
    /// Whether the text is in merge order, which editing it at its ends relies on.
    ordered: bool,
}

/// What the text holds of one service's lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MergedService {
    /// The service's [`Service::rewrites`] when its lines were merged.
    rewrites: u64,
    /// The newest sequence merged.
    read_to_seq: u64,
    /// The capture time of the newest record merged.
    last_ms: u64,
}

impl AllLogs {
    pub(crate) fn new() -> Self {
        Self {
            log: LogText::default(),
            dirty: true,
            numbers: HashMap::new(),
            names: Vec::new(),
            tags: HashMap::new(),
            merged: HashMap::new(),
            last: None,
            start: None,
            ordered: false,
        }
    }

    /// Brings the pane's text up to date with the lines of `services`, in sidebar order, and
    /// lays it out for `wrap` at `wrap_width` columns.
    pub(crate) fn render_text(&mut self, services: &[Service], wrap: bool, wrap_width: u16) {
        let numbers = self.number(services);
        if !self.dirty {
            self.log.relayout(wrap, wrap_width);
        }
        if self.dirty || !self.append(services, &numbers) {
            self.rebuild(services, &numbers, wrap, wrap_width);
        }
        self.dirty = false;
    }

    /// The number of each of `services`, giving a service seen for the first time the next one.
    fn number(&mut self, services: &[Service]) -> Vec<u32> {
        services
            .iter()
            .map(|service| {
                let id = &service.snapshot.id;
                if let Some(number) = self.numbers.get(id) {
                    return *number;
                }
                let number = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
                self.numbers.insert(id.clone(), number);
                self.names.push(id.clone());
                number
            })
            .collect()
    }

    /// Builds the text from every line of `services`.
    fn rebuild(&mut self, services: &[Service], numbers: &[u32], wrap: bool, wrap_width: u16) {
        let names = &self.names;
        let start = start_record(names, services, numbers);
        let name_width = services
            .iter()
            .map(|service| Span::raw(service.snapshot.id.as_str()).width())
            .max()
            .unwrap_or_default()
            .min(MAX_TAG_NAME_WIDTH);
        self.tags = services
            .iter()
            .zip(numbers)
            .map(|(service, number)| (*number, tag(&service.snapshot.id, *number, name_width)))
            .collect();
        let records = merge(sources(services, numbers, |_| 0))
            .filter(|record| starts_at_or_after(names, record.key, start))
            .collect::<Vec<_>>();
        self.ordered = records.windows(2).all(|pair| match pair {
            [before, after] => order(names, before.key) < order(names, after.key),
            _ => true,
        });
        self.last = records.last().map(|record| record.key);
        self.start = start;
        let tags = &self.tags;
        self.log.rebuild_tagged(
            records.iter().filter_map(|record| tagged(tags, record)),
            wrap,
            wrap_width,
        );
        self.merged = services
            .iter()
            .zip(numbers)
            .map(|(service, number)| (*number, merged_service(service)))
            .collect();
    }

    /// Edits the text at its ends for the lines `services` logged and dropped since it was
    /// built, and returns whether that could be done.
    ///
    /// It cannot when a service changed its lines otherwise, such as its last record, or when a
    /// new line would not go at the end.
    fn append(&mut self, services: &[Service], numbers: &[u32]) -> bool {
        if !self.ordered || self.merged.len() != services.len() {
            return false;
        }
        let mut read_to = HashMap::new();
        for (service, number) in services.iter().zip(numbers) {
            match self.merged.get(number) {
                Some(merged) if merged.rewrites == service.rewrites => {
                    read_to.insert(*number, *merged);
                }
                _ => return false,
            }
        }
        let names = &self.names;
        let start = start_record(names, services, numbers);
        let start_moved_back = match (self.start, start) {
            (Some(before), Some(now)) => order(names, now) < order(names, before),
            (Some(_), None) => true,
            (None, _) => false,
        };
        if start_moved_back {
            return false;
        }

        // The lines each service logged since, which must be later than what it logged before.
        let new = merge(sources(services, numbers, |number| {
            read_to.get(&number).map_or(0, |merged| merged.read_to_seq)
        }))
        .collect::<Vec<_>>();
        let mut last_ms = read_to
            .iter()
            .map(|(number, merged)| (*number, merged.last_ms))
            .collect::<HashMap<_, _>>();
        for record in &new {
            let service_last_ms = last_ms.entry(record.key.id.service).or_default();
            if record.key.timestamp_unix_ms < *service_last_ms {
                return false;
            }
            *service_last_ms = record.key.timestamp_unix_ms;
        }
        let kept = new
            .iter()
            .filter(|record| starts_at_or_after(names, record.key, start))
            .collect::<Vec<_>>();
        if let (Some(first), Some(last)) = (kept.first(), self.last)
            && order(names, first.key) <= order(names, last)
        {
            return false;
        }

        // Every dropped line was older than the start, as it was older than its service's
        // oldest line, which is no later than the start.
        let dropped = start.map_or(0, |start| {
            self.log
                .leading_records(|key| order(names, key) < order(names, start))
        });
        if dropped > 0 || !kept.is_empty() {
            let tags = &self.tags;
            self.log.edit_tagged(
                dropped,
                kept.iter().filter_map(|record| tagged(tags, record)),
            );
        }
        if let Some(record) = kept.last() {
            self.last = Some(record.key);
        }
        self.start = start;
        self.merged = services
            .iter()
            .zip(numbers)
            .map(|(service, number)| (*number, merged_service(service)))
            .collect();
        true
    }
}

/// What the text holds of `service` once all its lines are merged.
fn merged_service(service: &Service) -> MergedService {
    let newest = service.cached_lines.back();
    MergedService {
        rewrites: service.rewrites,
        read_to_seq: newest.map_or(0, |line| line.seq),
        last_ms: newest.map_or(0, |line| line.timestamp_unix_ms),
    }
}

/// `record` with the tag of its service.
fn tagged<'a>(
    tags: &'a HashMap<u32, Vec<Span<'static>>>,
    record: &MergedRecord<'a>,
) -> Option<TaggedRecord<'a>> {
    Some(TaggedRecord {
        key: record.key,
        text: record.text,
        tag: tags.get(&record.key.id.service)?,
    })
}

/// Where a record goes among every service's records: by capture time, then by service ID,
/// then by sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Order<'a> {
    timestamp_unix_ms: u64,
    service: &'a str,
    seq: u64,
}

/// Where `key` goes, with `names` giving each service number's ID.
fn order(names: &[String], key: RecordKey) -> Order<'_> {
    Order {
        timestamp_unix_ms: key.timestamp_unix_ms,
        service: usize::try_from(key.id.service)
            .ok()
            .and_then(|number| names.get(number))
            .map_or("", String::as_str),
        seq: key.id.seq,
    }
}

/// Whether `key` goes at or after `start`, when there is one.
fn starts_at_or_after(names: &[String], key: RecordKey, start: Option<RecordKey>) -> bool {
    start.is_none_or(|start| order(names, key) >= order(names, start))
}

/// The record the text starts at: the oldest line of the service that dropped lines most
/// recently, or `None` while no service dropped any.
///
/// A service whose log no longer starts at its first record dropped lines, by its retention or
/// because a restart cleared them.
fn start_record(names: &[String], services: &[Service], numbers: &[u32]) -> Option<RecordKey> {
    services
        .iter()
        .zip(numbers)
        .filter(|(service, _)| service.first_retained_seq > Some(1))
        .filter_map(|(service, number)| {
            let oldest = service.cached_lines.front()?;
            Some(RecordKey {
                id: RecordId {
                    service: *number,
                    seq: oldest.seq,
                },
                timestamp_unix_ms: oldest.timestamp_unix_ms,
            })
        })
        .max_by(|left, right| order(names, *left).cmp(&order(names, *right)))
}

/// The spans that lead each line of the service `id` with `number`: its name in its color,
/// padded or cut to `name_width` columns, and a separator.
fn tag(id: &str, number: u32, name_width: usize) -> Vec<Span<'static>> {
    let color = usize::try_from(number)
        .ok()
        .and_then(|number| TAG_COLORS.get(number % TAG_COLORS.len()))
        .copied()
        .unwrap_or(Color::Cyan);
    let name = fit_name(id, name_width);
    let padding = " ".repeat(name_width.saturating_sub(Span::raw(name.as_str()).width()));
    vec![
        format!("{name}{padding}").fg(color).bold(),
        " │ ".fg(Color::DarkGray),
    ]
}

/// `id` cut to `width` columns in the middle, an ellipsis taking a column, so names that share
/// a long prefix, such as `payments-service-api` and `payments-service-worker`, keep the end
/// that tells them apart.
fn fit_name(id: &str, width: usize) -> String {
    if Span::raw(id).width() <= width {
        return id.to_string();
    }
    let Some(room) = width.checked_sub(1) else {
        return String::new();
    };
    let char_width = |ch: char| Span::raw(ch.to_string()).width();
    let take = |chars: &mut dyn Iterator<Item = char>, columns: usize| {
        let mut taken = Vec::new();
        let mut used = 0;
        for ch in chars {
            if used + char_width(ch) > columns {
                break;
            }
            used += char_width(ch);
            taken.push(ch);
        }
        taken
    };
    let head = take(&mut id.chars(), room / 2);
    let mut tail = take(&mut id.chars().rev(), room - room / 2);
    tail.reverse();
    head.into_iter()
        .chain(std::iter::once('…'))
        .chain(tail)
        .collect()
}

/// A record of one service in the merged order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MergedRecord<'a> {
    key: RecordKey,
    text: &'a str,
}

/// The lines one service gives a merge.
struct Source<'a> {
    number: u32,
    name: &'a str,
    lines: std::iter::Peekable<std::collections::vec_deque::Iter<'a, CachedLine>>,
}

impl Source<'_> {
    /// The key of the service's next line.
    fn next_key(&mut self) -> Option<RecordKey> {
        let line = self.lines.peek()?;
        Some(RecordKey {
            id: RecordId {
                service: self.number,
                seq: line.seq,
            },
            timestamp_unix_ms: line.timestamp_unix_ms,
        })
    }
}

/// The lines of `services`, numbered `numbers`, after the sequence `after` gives for each
/// number.
fn sources<'a>(
    services: &'a [Service],
    numbers: &[u32],
    after: impl Fn(u32) -> u64,
) -> Vec<Source<'a>> {
    services
        .iter()
        .zip(numbers)
        .map(|(service, number)| {
            let after = after(*number);
            let first = service
                .cached_lines
                .partition_point(|line| line.seq <= after);
            Source {
                number: *number,
                name: &service.snapshot.id,
                lines: service.cached_lines.range(first..).peekable(),
            }
        })
        .collect()
}

/// The lines of `sources` merged by capture time, then by service ID, keeping each service's
/// own order.
///
/// The model stamps a service's records with capture times that never go back, so the merged
/// records come in [`Order`].
fn merge(mut sources: Vec<Source<'_>>) -> impl Iterator<Item = MergedRecord<'_>> {
    std::iter::from_fn(move || {
        // A sidebar holds few services, so the next record is the least of their next ones.
        let (index, key, _) = sources
            .iter_mut()
            .enumerate()
            .filter_map(|(index, source)| Some((index, source.next_key()?, source.name)))
            .min_by_key(|(_, key, name)| (key.timestamp_unix_ms, *name, key.id.seq))?;
        let line = sources.get_mut(index)?.lines.next()?;
        Some(MergedRecord {
            key,
            text: &line.text,
        })
    })
}

#[cfg(test)]
mod tests;
