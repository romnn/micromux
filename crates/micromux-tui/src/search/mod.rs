//! Searching the selected service's logs with `/` and moving between the matches with `n` and
//! `N`.
//!
//! The search runs over the text the log pane draws, after ANSI colors are parsed and JSON
//! records are formatted, so it finds what the pane shows, for a local session and an attached
//! one alike.
//! A query matches literally, and smartcase decides about case: a query without an uppercase
//! letter ignores case, one with an uppercase letter matches it exactly.
//!
//! The newest lines are at the bottom of the pane, so a search goes back in time, as vim's `?`
//! does: confirming a query moves to the newest match at or above the bottom of the view, `n` to
//! the next older match and `N` to the next newer one, both wrapping around.
//!
//! Matches are found again whenever the pane's text changes, and the current match stays on its
//! text while new lines arrive and old ones are dropped.

use std::ops::Range;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget as _, Wrap};
use unicode_segmentation::UnicodeSegmentation as _;

use crate::App;
use crate::state::{RecordLine, Service};

#[cfg(test)]
mod tests;

/// Longest query the prompt takes, which keeps matching and drawing the prompt cheap whatever
/// is pasted into it.
const MAX_QUERY_CHARS: usize = 256;

/// What a search looks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Query {
    text: String,
    /// The characters to find, each folded to lowercase when the query ignores case.
    needle: Vec<char>,
    /// Whether the query matches case exactly, which smartcase decides from its letters.
    case_sensitive: bool,
}

impl Query {
    /// A query for `text`, or `None` when it is empty.
    pub(crate) fn new(text: &str) -> Option<Self> {
        if text.is_empty() {
            return None;
        }
        let case_sensitive = text.chars().any(char::is_uppercase);
        let needle = if case_sensitive {
            text.chars().collect()
        } else {
            text.chars().map(fold_case).collect()
        };
        Some(Self {
            text: text.to_string(),
            needle,
            case_sensitive,
        })
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// The byte ranges of `haystack` the query matches, from left to right and none overlapping,
    /// each widened to whole graphemes.
    pub(crate) fn find_in(&self, haystack: &str) -> Vec<Range<usize>> {
        let found = self.find_chars_in(haystack);
        // Every ASCII character but the CR of a CRLF, which a log line never ends in, is a
        // grapheme of its own.
        if found.is_empty() || haystack.is_ascii() {
            return found;
        }
        snap_to_graphemes(haystack, found)
    }

    /// The byte ranges of `haystack` whose characters match the query.
    fn find_chars_in(&self, haystack: &str) -> Vec<Range<usize>> {
        if self.case_sensitive {
            return haystack
                .match_indices(self.text.as_str())
                .map(|(start, found)| start..start + found.len())
                .collect();
        }
        let mut found = Vec::new();
        let mut free_from = 0;
        for (start, _) in haystack.char_indices() {
            if start < free_from {
                continue;
            }
            if let Some(end) = self.folded_match_at(haystack, start) {
                found.push(start..end);
                free_from = end;
            }
        }
        found
    }

    /// The end of a case-insensitive match that starts at byte `start` of `haystack`, if one
    /// does.
    fn folded_match_at(&self, haystack: &str, start: usize) -> Option<usize> {
        let mut rest = haystack.get(start..)?.char_indices();
        let mut end = start;
        for wanted in &self.needle {
            let (offset, ch) = rest.next()?;
            if fold_case(ch) != *wanted {
                return None;
            }
            end = start + offset + ch.len_utf8();
        }
        Some(end)
    }
}

/// Widens each of `found`, ordered and apart, to the graphemes it touches, and merges those that
/// then overlap.
///
/// A highlight that split a grapheme, such as an `e` and its combining accent, would draw the
/// two halves apart and change how the line wraps.
fn snap_to_graphemes(haystack: &str, found: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let boundaries = haystack
        .grapheme_indices(true)
        .map(|(start, _)| start)
        .chain(std::iter::once(haystack.len()))
        .collect::<Vec<_>>();
    let mut snapped: Vec<Range<usize>> = Vec::with_capacity(found.len());
    for range in found {
        let start = boundaries
            .partition_point(|boundary| *boundary <= range.start)
            .checked_sub(1)
            .and_then(|index| boundaries.get(index))
            .copied()
            .unwrap_or(range.start);
        let end = boundaries
            .get(boundaries.partition_point(|boundary| *boundary < range.end))
            .copied()
            .unwrap_or(range.end);
        match snapped.last_mut() {
            Some(last) if start < last.end => last.end = last.end.max(end),
            _ => snapped.push(start..end),
        }
    }
    snapped
}

/// `ch` in lowercase where that is a single character, so a match covers as many characters as
/// the query has; `ch` itself where lowercasing expands it, as for `İ`.
fn fold_case(ch: char) -> char {
    let mut lower = ch.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => ch,
    }
}

/// A run of the log pane's text that a query matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Match {
    /// The line of the pane's text.
    pub line: usize,
    /// The bytes of the line's text, its spans' contents joined.
    pub bytes: Range<usize>,
}

/// Every match of `query` in `text`, ordered by line and then by position in the line.
pub(crate) fn find_matches(text: &ratatui::text::Text<'_>, query: &Query) -> Vec<Match> {
    let mut matches = Vec::new();
    for (line, content) in text.lines.iter().enumerate() {
        let haystack = match content.spans.as_slice() {
            [single] => std::borrow::Cow::Borrowed(single.content.as_ref()),
            spans => std::borrow::Cow::Owned(
                spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>(),
            ),
        };
        matches.extend(
            query
                .find_in(&haystack)
                .into_iter()
                .map(|bytes| Match { line, bytes }),
        );
    }
    matches
}

/// The rendered row `found` starts on in `service`'s text, under the wrapping the text was
/// last indexed for.
fn match_row(service: &Service, found: &Match) -> Option<usize> {
    let first_row = service.cached_line_index.line_start(found.line)?;
    let Some((true, width)) = service.cached_wrap else {
        return Some(first_row);
    };
    let line = service.cached_text.lines.get(found.line)?;
    Some(first_row + row_in_wrapped_line(line, &found.bytes, width))
}

/// The row that byte `bytes.start` of `line` is drawn on when the line wraps at `width` columns
/// as the log pane wraps it.
///
/// Wrapping depends on the text alone, so the line is drawn off screen with only the match
/// styled, and the first styled cell gives the row.
fn row_in_wrapped_line(line: &Line<'_>, bytes: &Range<usize>, width: u16) -> usize {
    let content = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    let (Some(before), Some(matched), Some(after)) = (
        content.get(..bytes.start),
        content.get(bytes.clone()),
        content.get(bytes.end..),
    ) else {
        return 0;
    };
    let marked = Line::from(vec![
        Span::raw(before),
        Span::styled(matched, Style::new().add_modifier(Modifier::REVERSED)),
        Span::raw(after),
    ]);
    let paragraph = Paragraph::new(marked).wrap(Wrap { trim: false });
    let height = u16::try_from(paragraph.line_count(width)).unwrap_or(u16::MAX);
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    paragraph.render(area, &mut buf);
    area.positions()
        .find(|position| {
            buf.cell(*position)
                .is_some_and(|cell| cell.modifier.contains(Modifier::REVERSED))
        })
        .map_or(0, |position| usize::from(position.y))
}

/// Which way a search moves through the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    /// Up, toward older lines, as `n` moves.
    Older,
    /// Down, toward newer lines, as `N` moves.
    Newer,
}

/// The rendered rows of the log pane on screen, both ends included.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ViewRows {
    pub top: usize,
    pub bottom: usize,
}

/// The search of the log pane: the prompt being typed, and the confirmed query with its matches.
#[derive(Debug, Default)]
pub(crate) struct LogSearch {
    /// What has been typed after `/`, while the prompt is open.
    pub prompt: Option<String>,
    active: Option<ActiveSearch>,
}

/// A confirmed query and where it matches.
#[derive(Debug)]
struct ActiveSearch {
    query: Query,
    matches: Vec<Match>,
    /// The text `matches` were found in, or `None` before they were looked for.
    found_in: Option<FoundIn>,
    /// The match moved to last, while it is still there.
    current: Option<CurrentMatch>,
}

/// Which service's text, at which rebuild, matches were found in.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FoundIn {
    service_id: String,
    text_generation: u64,
}

#[derive(Debug, Clone, Copy)]
struct CurrentMatch {
    /// The index into the matches.
    index: usize,
    anchor: MatchAnchor,
}

/// Where a match is, by a key that stays with its text while lines arrive and old ones go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct MatchAnchor {
    record_line: RecordLine,
    start: usize,
}

impl MatchAnchor {
    fn of(found: &Match, service: &Service) -> Self {
        Self {
            // Every line of a rendered text belongs to a record, so the fallback is never taken.
            record_line: service.record_line(found.line).unwrap_or(RecordLine {
                seq: 0,
                line: found.line,
            }),
            start: found.bytes.start,
        }
    }
}

/// What the log pane's border says about the search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SearchStatus<'a> {
    pub query: &'a str,
    /// The index of the current match, if one was moved to.
    pub current: Option<usize>,
    pub count: usize,
}

impl LogSearch {
    /// Whether a query is confirmed, which `n`, `N` and `Esc` act on.
    pub(crate) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// Confirms `query`, whose matches are looked for on the next refresh.
    fn start(&mut self, query: Query) {
        self.active = Some(ActiveSearch {
            query,
            matches: Vec::new(),
            found_in: None,
            current: None,
        });
    }

    /// Ends the search and its highlighting.
    pub(crate) fn clear(&mut self) {
        self.active = None;
    }

    /// Finds the matches again when `service`'s text is not the one they were found in.
    ///
    /// In the same service the current match follows its text; when its text is gone, the
    /// nearest newer match becomes current, or the newest one when none is newer.
    /// In another service no match is current until `n` or `N` moves to one.
    pub(crate) fn refresh(&mut self, service: &Service) {
        let Some(active) = &mut self.active else {
            return;
        };
        let found_in = FoundIn {
            service_id: service.snapshot.id.clone(),
            text_generation: service.text_generation,
        };
        if active.found_in.as_ref() == Some(&found_in) {
            return;
        }
        let same_service = active
            .found_in
            .as_ref()
            .is_some_and(|previous| previous.service_id == found_in.service_id);
        active.matches = find_matches(&service.cached_text, &active.query);
        active.current = active.current.filter(|_| same_service).and_then(|current| {
            // Matches are ordered by line, and so by anchor.
            let index = active
                .matches
                .partition_point(|found| MatchAnchor::of(found, service) < current.anchor)
                .min(active.matches.len().checked_sub(1)?);
            let found = active.matches.get(index)?;
            Some(CurrentMatch {
                index,
                anchor: MatchAnchor::of(found, service),
            })
        });
        active.found_in = Some(found_in);
    }

    /// Moves to the next match in `direction` and returns the rendered row it starts on, or
    /// `None` without matches.
    ///
    /// Without a current match, `Older` moves to the newest match at or above the bottom of
    /// `view` and `Newer` to the oldest at or below its top; past either end it wraps around.
    /// `service` must be the one the matches were refreshed for.
    pub(crate) fn step(
        &mut self,
        direction: Direction,
        service: &Service,
        view: ViewRows,
    ) -> Option<usize> {
        let active = self.active.as_mut()?;
        let count = active.matches.len();
        let last = count.checked_sub(1)?;
        let row_of = |found: &Match| match_row(service, found);
        let wrap_newer = |index: usize| if index < count { index } else { 0 };
        let index = match (active.current, direction) {
            (Some(current), Direction::Older) => current.index.checked_sub(1).unwrap_or(last),
            (Some(current), Direction::Newer) => wrap_newer(current.index + 1),
            (None, Direction::Older) => active
                .matches
                .partition_point(|found| row_of(found).is_some_and(|row| row <= view.bottom))
                .checked_sub(1)
                .unwrap_or(last),
            (None, Direction::Newer) => wrap_newer(
                active
                    .matches
                    .partition_point(|found| row_of(found).is_some_and(|row| row < view.top)),
            ),
        };
        let found = active.matches.get(index)?;
        active.current = Some(CurrentMatch {
            index,
            anchor: MatchAnchor::of(found, service),
        });
        match_row(service, found)
    }

    /// What the log pane's border says, while a query is confirmed.
    pub(crate) fn status(&self) -> Option<SearchStatus<'_>> {
        let active = self.active.as_ref()?;
        Some(SearchStatus {
            query: active.query.text(),
            current: active.current.map(|current| current.index),
            count: active.matches.len(),
        })
    }

    /// The matches the log pane highlights.
    pub(crate) fn highlights(&self) -> crate::render::log_view::Highlights<'_> {
        self.active
            .as_ref()
            .map(|active| crate::render::log_view::Highlights {
                matches: &active.matches,
                current: active.current.map(|current| current.index),
            })
            .unwrap_or_default()
    }
}

impl App {
    /// Opens the prompt for a query, as `/` does.
    pub(crate) fn open_search_prompt(&mut self) {
        self.search.prompt = Some(String::new());
    }

    /// Keys while the search prompt is open, which all edit the query: `Enter` confirms it, and
    /// `Esc`, or `Backspace` once it is empty, closes the prompt and keeps the previous search.
    pub(crate) fn handle_key_press_search_prompt(&mut self, code: crossterm::event::KeyCode) {
        use crossterm::event::KeyCode;

        let Some(prompt) = &mut self.search.prompt else {
            return;
        };
        match code {
            KeyCode::Char(ch) => {
                if prompt.chars().count() < MAX_QUERY_CHARS {
                    prompt.push(ch);
                }
            }
            KeyCode::Backspace => {
                if prompt.pop().is_none() {
                    self.search.prompt = None;
                }
            }
            KeyCode::Esc => self.search.prompt = None,
            KeyCode::Enter => {
                let text = self.search.prompt.take().unwrap_or_default();
                if let Some(query) = Query::new(&text) {
                    self.search.start(query);
                    self.jump_to_match(Direction::Older);
                }
            }
            _ => {}
        }
    }

    /// Adds pasted text to the search prompt, up to its first line break.
    pub(crate) fn paste_into_search_prompt(&mut self, text: &str) {
        if let Some(prompt) = &mut self.search.prompt {
            let first_line = text.lines().next().unwrap_or_default();
            let room = MAX_QUERY_CHARS.saturating_sub(prompt.chars().count());
            prompt.extend(first_line.chars().filter(|ch| !ch.is_control()).take(room));
        }
    }

    /// Moves to the next match in `direction` and scrolls the log pane to it.
    ///
    /// The view stops following the tail, so the match stays where it was put while new lines
    /// arrive.
    pub(crate) fn jump_to_match(&mut self, direction: Direction) {
        let viewport = usize::from(self.log_viewport_height()).max(1);
        let Some(service) = self.state.current_service() else {
            return;
        };
        self.search.refresh(service);
        let total = service.cached_line_index.total_lines();
        let max_offset = total.saturating_sub(viewport);
        let offset = if self.log_view.follow_tail {
            max_offset
        } else {
            self.log_view.scroll_offset.min(max_offset)
        };
        let view = ViewRows {
            top: offset,
            bottom: offset + viewport - 1,
        };
        let Some(row) = self.search.step(direction, service, view) else {
            return;
        };
        // A match on screen leaves the view where it is; one off screen comes to its middle.
        self.log_view.scroll_offset = if (view.top..=view.bottom).contains(&row) {
            offset
        } else {
            row.saturating_sub(viewport / 2).min(max_offset)
        };
        self.log_view.follow_tail = false;
    }
}
