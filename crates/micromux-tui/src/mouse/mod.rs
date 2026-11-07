//! Mouse support: what the last frame drew where, and what a press, a drag, a release or a turn
//! of the wheel does there.
//!
//! The renderer records every place the mouse can act on in [`Hits`] while it draws, so a click
//! lands on exactly what the frame showed there.
//! Under a popup only the popup reacts, and a press outside it closes it.
//!
//! Text is selected inside one pane at a time, never across panes: a drag selects from where it
//! started, a double click a word and a triple click a line, and releasing the button copies the
//! selection to the clipboard.
//! A drag past the top or bottom of a pane scrolls it a row at a time.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget as _};

use crate::{App, Effect, Focus, InfoView};

/// Rows one turn of the wheel scrolls a pane, as terminals scroll their own.
const WHEEL_ROWS: u16 = 3;

/// How soon a second press on the same cell counts as a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Fills the cells of an off-screen render that no text was drawn into, so copying tells
/// padding and the hidden half of a wide character apart from a real space.
const UNWRITTEN: &str = "\0";

/// What the last frame drew where, for the mouse to land on.
#[derive(Debug, Default)]
pub(crate) struct Hits {
    /// The services pane, borders included.
    pub(crate) services: Rect,
    /// The rows of the services list.
    pub(crate) service_rows: Rect,
    /// Index of the service on the first row of the list.
    pub(crate) first_service: usize,
    /// The logs pane, borders and scrollbar included.
    pub(crate) logs: Rect,
    /// The healthcheck pane, or an empty area while it is hidden.
    pub(crate) health: Rect,
    /// Key hints in the header and footer that a click runs.
    pub(crate) hints: Vec<HintHit>,
    /// URLs a click opens in the browser.
    pub(crate) links: Vec<LinkHit>,
    /// The popup on top of everything, if any.
    pub(crate) popup: Option<PopupHit>,
}

/// A key hint and the cells it was drawn in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HintHit {
    pub(crate) area: Rect,
    /// The keys as the hint shows them, such as `?` or `Tab`.
    pub(crate) keys: &'static str,
}

/// A URL and the cells it was drawn in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkHit {
    pub(crate) area: Rect,
    pub(crate) url: String,
}

/// The popup of the last frame.
#[derive(Debug)]
pub(crate) struct PopupHit {
    /// The whole popup, borders included; a press outside it closes it.
    pub(crate) area: Rect,
    pub(crate) content: PopupContent,
}

/// What a popup shows, for the mouse to act on.
#[derive(Debug)]
pub(crate) enum PopupContent {
    /// Scrollable text: the help or a details popup, with every row it can scroll through.
    Text {
        pane: TextPane,
        area: Rect,
        lines: Vec<Line<'static>>,
    },
    /// The level picker's choices, one per row.
    Levels {
        rows: Rect,
        /// Index of the choice on the first row.
        first: usize,
    },
}

/// A pane whose text can be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextPane {
    Logs,
    Healthcheck,
    Help,
    Info(InfoView),
}

/// A place in a pane's text: a rendered row counted from the top of everything the pane can
/// scroll through, and a cell column within the pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct TextPos {
    pub(crate) row: usize,
    pub(crate) col: u16,
}

/// What a pane's rows belong to; a selection made under one context means nothing under another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextContext {
    pub(crate) pane: TextPane,
    /// The service whose text the pane shows, if any.
    pub(crate) service: Option<String>,
    pub(crate) wrap: bool,
    pub(crate) width: u16,
}

/// Selected text: the cells from `anchor` to `head` in text order, both included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) context: TextContext,
    pub(crate) anchor: TextPos,
    pub(crate) head: TextPos,
}

impl Selection {
    fn ordered(&self) -> (TextPos, TextPos) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Whether nothing is selected: a click without a drag.
    fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// The selected columns of `row` in a pane `width` columns wide, both ends included.
    fn columns_in_row(&self, row: usize, width: u16) -> Option<(u16, u16)> {
        let (start, end) = self.ordered();
        if self.is_empty() || row < start.row || row > end.row || width == 0 {
            return None;
        }
        let from = if row == start.row { start.col } else { 0 };
        let to = if row == end.row { end.col } else { width - 1 };
        Some((from, to.min(width - 1)))
    }
}

/// The last column of row `y` of `buf` that shows text, counting the hidden half of a wide
/// character there, or `None` for a row without any.
///
/// Blanks after the text are padding, such as the spaces a service's terminal output fills its
/// rows with, so a selection neither highlights nor copies them.
fn text_end(buf: &Buffer, y: u16, width: u16) -> Option<u16> {
    (0..width).rev().find_map(|x| {
        let symbol = buf.cell((x, y))?.symbol();
        (symbol != UNWRITTEN && !symbol.trim().is_empty()).then(|| {
            let width = ratatui::text::Span::raw(symbol).width();
            x.saturating_add(crate::render::rows_u16(width.saturating_sub(1)))
        })
    })
}

/// What a press inside or outside the popup lands on.
enum PopupPress {
    Outside,
    Level(usize),
    Text(TextPane),
    /// The popup's border or padding.
    Inert,
}

/// The last press of the left button, to count a quick second one on the same cell.
#[derive(Debug, Clone, Copy)]
struct Press {
    at: Instant,
    position: Position,
    count: usize,
}

/// The mouse's part of the app state.
#[derive(Debug)]
pub(crate) struct MouseState {
    /// Whether the terminal reports the mouse to the TUI.
    pub(crate) capture: bool,
    pub(crate) selection: Option<Selection>,
    /// Whether the left button is down on a selection being made.
    dragging: bool,
    last_press: Option<Press>,
    /// What the last release copied, shown in the footer until the next press or key.
    pub(crate) notice: Option<String>,
}

impl MouseState {
    pub(crate) fn new(capture: bool) -> Self {
        Self {
            capture,
            selection: None,
            dragging: false,
            last_press: None,
            notice: None,
        }
    }

    /// Counts a press at `position` in a quick series on the same cell and returns which press
    /// of the series it is.
    fn count_press(&mut self, position: Position, now: Instant) -> usize {
        let count = match self.last_press {
            Some(last) if last.position == position && now - last.at < DOUBLE_CLICK => {
                last.count + 1
            }
            _ => 1,
        };
        self.last_press = Some(Press {
            at: now,
            position,
            count,
        });
        count
    }
}

/// The key a click on the hint for `keys` presses, or `None` when the hint names more than one
/// key or its key ends the session, which a stray click must not do.
fn hint_key(keys: &str) -> Option<KeyEvent> {
    let key = |code, modifiers| KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    };
    match keys {
        "q" => None,
        "Tab" => Some(key(KeyCode::Tab, KeyModifiers::NONE)),
        "Alt+Esc" => Some(key(KeyCode::Esc, KeyModifiers::ALT)),
        _ => {
            let mut chars = keys.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => Some(key(KeyCode::Char(ch), KeyModifiers::NONE)),
                _ => None,
            }
        }
    }
}

impl App {
    /// Turns mouse capture on or off, as the `m` key does.
    pub(crate) fn toggle_mouse_capture(&mut self) {
        let capture = !self.mouse.capture;
        self.mouse.capture = capture;
        self.mouse.selection = None;
        self.mouse.dragging = false;
        self.effects.push(Effect::MouseCapture(capture));
    }

    /// Acts on a mouse event from the terminal.
    pub(crate) fn handle_mouse(&mut self, event: MouseEvent) {
        // The terminal may still report events it sent before capture went off.
        if !self.mouse.capture {
            return;
        }
        let position = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.press(position, Instant::now()),
            MouseEventKind::Drag(MouseButton::Left) => self.drag_to(position),
            MouseEventKind::Up(MouseButton::Left) => self.release(),
            MouseEventKind::ScrollUp => self.wheel(position, false),
            MouseEventKind::ScrollDown => self.wheel(position, true),
            _ => {}
        }
    }

    /// A press of the left button at `position`, `now`.
    fn press(&mut self, position: Position, now: Instant) {
        let count = self.mouse.count_press(position, now);
        self.mouse.notice = None;
        self.mouse.selection = None;
        self.mouse.dragging = false;

        // A popup takes every press: one outside closes it, as Esc does.
        if let Some(popup) = &self.hits.popup {
            let target = if popup.area.contains(position) {
                match &popup.content {
                    PopupContent::Levels { rows, first } if rows.contains(position) => {
                        PopupPress::Level(first + usize::from(position.y - rows.y))
                    }
                    PopupContent::Levels { .. } => PopupPress::Inert,
                    PopupContent::Text { pane, .. } => PopupPress::Text(*pane),
                }
            } else {
                PopupPress::Outside
            };
            match target {
                PopupPress::Outside => self.close_popup(),
                PopupPress::Level(index) => self.pick_level(index),
                PopupPress::Text(pane) => self.start_selection(pane, position, count),
                PopupPress::Inert => {}
            }
            return;
        }

        if let Some(keys) = self
            .hits
            .hints
            .iter()
            .find(|hint| hint.area.contains(position))
            .map(|hint| hint.keys)
        {
            self.click_hint(keys);
            return;
        }
        if let Some(url) = self
            .hits
            .links
            .iter()
            .find(|link| link.area.contains(position))
            .map(|link| link.url.clone())
        {
            self.effects.push(Effect::OpenUrl(url));
            return;
        }

        if self.hits.services.contains(position) {
            self.focus = Focus::Services;
            // Typed input goes to the selected service, so a click must not retarget it.
            if !self.pty_input_mode && self.hits.service_rows.contains(position) {
                let index =
                    self.hits.first_service + usize::from(position.y - self.hits.service_rows.y);
                if index < self.state.services.len() {
                    self.state.selected_service = index;
                }
            }
        } else if self.hits.logs.contains(position) {
            self.focus = Focus::Logs;
            self.start_selection(TextPane::Logs, position, count);
        } else if self.hits.health.contains(position) {
            self.focus = Focus::Healthcheck;
            self.start_selection(TextPane::Healthcheck, position, count);
        }
    }

    /// Runs the key of the hint for `keys`, as if it were pressed.
    fn click_hint(&mut self, keys: &'static str) {
        let Some(key) = hint_key(keys) else {
            return;
        };
        // In PTY input mode keys go to the service, so only the hint that leaves the mode acts.
        if self.pty_input_mode && keys != "Alt+Esc" {
            return;
        }
        self.handle_key_press(key);
    }

    /// Closes whichever popup is on screen.
    fn close_popup(&mut self) {
        self.show_help = false;
        self.info = None;
        self.level_picker = None;
    }

    /// Applies the level picker's choice at `index`, as moving there and pressing Enter does.
    fn pick_level(&mut self, index: usize) {
        let Some(picker) = &mut self.level_picker else {
            return;
        };
        if index >= crate::level::LEVEL_CHOICES.len() {
            return;
        }
        picker.cursor = index;
        self.handle_key_press_level_picker(KeyCode::Enter);
    }

    /// Starts a selection in `pane` at `position`, the `count`th press of a quick series: the
    /// first places a caret, the second selects the word there and the third its line.
    fn start_selection(&mut self, pane: TextPane, position: Position, count: usize) {
        let (Some(context), Some(area)) = (self.text_context(pane), self.text_area(pane)) else {
            return;
        };
        if !area.contains(position) {
            return;
        }
        let pos = TextPos {
            row: self.first_row(pane) + usize::from(position.y - area.y),
            col: position.x - area.x,
        };
        let (anchor, head) = match count {
            1 => (pos, pos),
            2 => self.word_at(&context, pos),
            _ => self.line_at(pane, &context, pos),
        };
        self.mouse.selection = Some(Selection {
            context,
            anchor,
            head,
        });
        self.mouse.dragging = true;
    }

    /// The left button moved to `position` while down: the selection extends there, and past the
    /// pane's top or bottom the pane scrolls a row.
    fn drag_to(&mut self, position: Position) {
        if !self.mouse.dragging {
            return;
        }
        let Some(pane) = self
            .mouse
            .selection
            .as_ref()
            .map(|selection| selection.context.pane)
        else {
            return;
        };
        let Some(area) = self.text_area(pane) else {
            return;
        };
        if area.is_empty() {
            return;
        }
        let edge_row = if position.y < area.top() {
            self.scroll_pane(pane, false, 1);
            0
        } else if position.y >= area.bottom() {
            self.scroll_pane(pane, true, 1);
            area.height - 1
        } else {
            position.y - area.y
        };
        let last_row = self.total_rows(pane).saturating_sub(1);
        let head = TextPos {
            row: (self.first_row(pane) + usize::from(edge_row)).min(last_row),
            col: position.x.saturating_sub(area.x).min(area.width - 1),
        };
        if let Some(selection) = &mut self.mouse.selection {
            selection.head = head;
        }
    }

    /// The left button came up: a selection being made ends, and its text goes to the clipboard.
    fn release(&mut self) {
        if !std::mem::take(&mut self.mouse.dragging) {
            return;
        }
        let Some(text) = self.selected_text() else {
            return;
        };
        let lines = text.lines().count().max(1);
        self.mouse.notice = Some(if lines == 1 {
            format!("copied {} characters", text.chars().count())
        } else {
            format!("copied {lines} lines")
        });
        self.effects.push(Effect::Copy(text));
    }

    /// A turn of the wheel at `position`: under a popup it scrolls the popup, elsewhere the pane
    /// under the pointer, without moving the focus.
    fn wheel(&mut self, position: Position, down: bool) {
        let popup = self.hits.popup.as_ref().map(|popup| match popup.content {
            PopupContent::Levels { .. } => None,
            PopupContent::Text { pane, .. } => Some(pane),
        });
        match popup {
            Some(None) => {
                let key = if down { KeyCode::Down } else { KeyCode::Up };
                self.handle_key_press_level_picker(key);
                return;
            }
            Some(Some(pane)) => {
                self.scroll_pane(pane, down, WHEEL_ROWS);
                return;
            }
            None => {}
        }
        if self.hits.services.contains(position) {
            if down {
                self.state.service_down();
            } else {
                self.state.service_up();
            }
        } else if self.hits.logs.contains(position) {
            self.scroll_pane(TextPane::Logs, down, WHEEL_ROWS);
        } else if self.hits.health.contains(position) {
            self.scroll_pane(TextPane::Healthcheck, down, WHEEL_ROWS);
        }
    }

    /// Scrolls `pane` by `rows`, as the movement keys do.
    fn scroll_pane(&mut self, pane: TextPane, down: bool, rows: u16) {
        match (pane, down) {
            (TextPane::Logs, false) => self.scroll_logs_up(rows),
            (TextPane::Logs, true) => self.scroll_logs_down(rows),
            (TextPane::Healthcheck, false) => self.scroll_healthchecks_up(rows),
            (TextPane::Healthcheck, true) => self.scroll_healthchecks_down(rows),
            (TextPane::Help | TextPane::Info(_), false) => {
                self.popup_scroll = self.popup_scroll.saturating_sub(usize::from(rows));
            }
            // Rendering clamps it to the last page.
            (TextPane::Help | TextPane::Info(_), true) => {
                self.popup_scroll = self.popup_scroll.saturating_add(usize::from(rows));
            }
        }
    }

    /// Where `pane`'s text was drawn in the last frame.
    fn text_area(&self, pane: TextPane) -> Option<Rect> {
        match pane {
            TextPane::Logs => Some(crate::render::log_view::text_area(self.hits.logs)),
            TextPane::Healthcheck => Some(crate::render::log_view::text_area(self.hits.health)),
            TextPane::Help | TextPane::Info(_) => match &self.hits.popup {
                Some(PopupHit {
                    content:
                        PopupContent::Text {
                            pane: shown, area, ..
                        },
                    ..
                }) if *shown == pane => Some(*area),
                _ => None,
            },
        }
    }

    /// What `pane`'s rows belong to now, or `None` when the pane is not on screen.
    pub(crate) fn text_context(&self, pane: TextPane) -> Option<TextContext> {
        let area = self.text_area(pane).filter(|area| !area.is_empty())?;
        let service = self
            .state
            .current_service()
            .map(|service| service.snapshot.id.clone());
        let (service, wrap) = match pane {
            TextPane::Logs => (Some(service?), self.log_view.wrap),
            TextPane::Healthcheck => (Some(service?), self.healthcheck_view.wrap),
            TextPane::Info(InfoView::Service) => (service, false),
            TextPane::Help | TextPane::Info(InfoView::Session) => (None, false),
        };
        Some(TextContext {
            pane,
            service,
            wrap,
            width: area.width,
        })
    }

    /// The row drawn at the top of `pane`.
    fn first_row(&self, pane: TextPane) -> usize {
        match pane {
            TextPane::Logs => self.log_view.scroll_offset,
            TextPane::Healthcheck => self.healthcheck_view.scroll_offset,
            TextPane::Help | TextPane::Info(_) => self.popup_scroll,
        }
    }

    /// How many rows `pane` can scroll through.
    fn total_rows(&self, pane: TextPane) -> usize {
        match pane {
            TextPane::Logs => self
                .state
                .current_service()
                .map_or(0, |service| service.cached_line_index.total_lines()),
            TextPane::Healthcheck => self.state.current_service().map_or(0, |service| {
                service.healthcheck_cached_line_index.total_lines()
            }),
            TextPane::Help | TextPane::Info(_) => match &self.hits.popup {
                Some(PopupHit {
                    content: PopupContent::Text { lines, .. },
                    ..
                }) => lines.len(),
                _ => 0,
            },
        }
    }

    /// Draws `rows` of the text `context` describes off screen, exactly as the pane draws them,
    /// into a buffer whose cells without text hold [`UNWRITTEN`].
    fn draw_rows(&self, context: &TextContext, rows: std::ops::Range<usize>) -> Buffer {
        let height = crate::render::rows_u16(rows.len());
        let area = Rect::new(0, 0, context.width, height);
        let mut buf = Buffer::filled(area, Cell::new(UNWRITTEN));
        let service = self.state.current_service();
        match context.pane {
            TextPane::Logs => {
                if let Some(service) = service {
                    crate::render::log_view::content_paragraph(
                        &service.cached_text,
                        &service.cached_line_index,
                        rows.start,
                        rows.len(),
                        context.wrap,
                    )
                    .render(area, &mut buf);
                }
            }
            TextPane::Healthcheck => {
                if let Some(service) = service {
                    crate::render::log_view::content_paragraph(
                        &service.healthcheck_cached_text,
                        &service.healthcheck_cached_line_index,
                        rows.start,
                        rows.len(),
                        context.wrap,
                    )
                    .render(area, &mut buf);
                }
            }
            TextPane::Help | TextPane::Info(_) => {
                if let Some(PopupHit {
                    content: PopupContent::Text { lines, .. },
                    ..
                }) = &self.hits.popup
                {
                    let shown = lines.get(rows).unwrap_or_default().to_vec();
                    Paragraph::new(shown).render(area, &mut buf);
                }
            }
        }
        buf
    }

    /// Whether `row` of `context`'s text starts a line rather than continuing a wrapped one.
    fn starts_line(&self, context: &TextContext, row: usize) -> bool {
        let service = self.state.current_service();
        match context.pane {
            TextPane::Logs => {
                service.is_none_or(|service| service.cached_line_index.starts_line(row))
            }
            TextPane::Healthcheck => {
                service.is_none_or(|service| service.healthcheck_cached_line_index.starts_line(row))
            }
            TextPane::Help | TextPane::Info(_) => true,
        }
    }

    /// The whitespace that wrapping dropped before rendered `row`, which continues a wrapped
    /// line.
    ///
    /// Word wrapping breaks a line at whitespace and draws none of it, so the text the line's
    /// rows show is found in the line in order, and what lies between two rows is the gap.
    fn wrap_gap(&self, context: &TextContext, row: usize) -> String {
        let Some(service) = self.state.current_service() else {
            return String::new();
        };
        let (text, index) = match context.pane {
            TextPane::Logs => (&service.cached_text, &service.cached_line_index),
            TextPane::Healthcheck => (
                &service.healthcheck_cached_text,
                &service.healthcheck_cached_line_index,
            ),
            TextPane::Help | TextPane::Info(_) => return String::new(),
        };
        let Some((line, first_row)) = index.line_of_row(row) else {
            return String::new();
        };
        let Some(source) = text.lines.get(line) else {
            return String::new();
        };
        // The line as the paragraph draws it: control characters and zero-width ones never land
        // in a cell.
        let drawn = source
            .styled_graphemes(ratatui::style::Style::default())
            .map(|grapheme| grapheme.symbol)
            .filter(|symbol| {
                !symbol.contains(char::is_control) && ratatui::text::Span::raw(*symbol).width() > 0
            })
            .collect::<String>();
        let buf = self.draw_rows(context, first_row..row + 1);
        let mut cursor = 0;
        let mut gap = "";
        for y in 0..crate::render::rows_u16(row + 1 - first_row) {
            let shown = (0..context.width)
                .filter_map(|x| buf.cell((x, y)).map(Cell::symbol))
                .filter(|symbol| *symbol != UNWRITTEN)
                .collect::<String>();
            let rest = drawn.get(cursor..).unwrap_or_default();
            let Some(at) = rest.find(&shown) else {
                return String::new();
            };
            gap = rest.get(..at).unwrap_or_default();
            cursor += at + shown.len();
        }
        gap.to_string()
    }

    /// The selected text, or `None` when nothing is selected; rows of one wrapped line join
    /// with the whitespace the wrap dropped between them.
    pub(crate) fn selected_text(&self) -> Option<String> {
        let selection = self
            .mouse
            .selection
            .as_ref()
            .filter(|selection| !selection.is_empty())?;
        let context = &selection.context;
        if self.text_context(context.pane).as_ref() != Some(context) {
            return None;
        }
        let (start, end) = selection.ordered();
        let buf = self.draw_rows(context, start.row..end.row + 1);
        let mut text = String::new();
        for row in start.row..=end.row {
            if row > start.row {
                if self.starts_line(context, row) {
                    text.push('\n');
                } else {
                    text.push_str(&self.wrap_gap(context, row));
                }
            }
            let Some((from, to)) = selection.columns_in_row(row, context.width) else {
                continue;
            };
            let y = crate::render::rows_u16(row - start.row);
            let Some(text_end) = text_end(&buf, y, context.width) else {
                continue;
            };
            for x in from..=to.min(text_end) {
                if let Some(cell) = buf.cell((x, y))
                    && cell.symbol() != UNWRITTEN
                {
                    text.push_str(cell.symbol());
                }
            }
        }
        (!text.is_empty()).then_some(text)
    }

    /// The word around `pos`: the run of non-blank cells it is in, or of blank ones.
    fn word_at(&self, context: &TextContext, pos: TextPos) -> (TextPos, TextPos) {
        let buf = self.draw_rows(context, pos.row..pos.row + 1);
        let blank = |x: u16| {
            buf.cell((x, 0))
                .is_none_or(|cell| cell.symbol() == UNWRITTEN || cell.symbol().trim().is_empty())
        };
        let kind = blank(pos.col);
        let mut from = pos.col;
        while from > 0 && blank(from - 1) == kind {
            from -= 1;
        }
        let mut to = pos.col;
        while to + 1 < context.width && blank(to + 1) == kind {
            to += 1;
        }
        (
            TextPos {
                row: pos.row,
                col: from,
            },
            TextPos {
                row: pos.row,
                col: to,
            },
        )
    }

    /// The whole line at `pos`, every row of it when it wraps.
    fn line_at(&self, pane: TextPane, context: &TextContext, pos: TextPos) -> (TextPos, TextPos) {
        let mut first = pos.row;
        while first > 0 && !self.starts_line(context, first) {
            first -= 1;
        }
        let last_row = self.total_rows(pane).saturating_sub(1).max(pos.row);
        let mut last = pos.row;
        while last < last_row && !self.starts_line(context, last + 1) {
            last += 1;
        }
        (
            TextPos { row: first, col: 0 },
            TextPos {
                row: last,
                col: context.width.saturating_sub(1),
            },
        )
    }

    /// Highlights the selected cells that the frame in `buf` shows, after dropping a selection
    /// whose text the panes no longer show, such as one in another service's logs.
    pub(crate) fn paint_selection(&mut self, buf: &mut Buffer) {
        let Some(selection) = &self.mouse.selection else {
            return;
        };
        let pane = selection.context.pane;
        if self.text_context(pane).as_ref() != Some(&selection.context) {
            self.mouse.selection = None;
            self.mouse.dragging = false;
            return;
        }
        let Some(area) = self.text_area(pane) else {
            return;
        };
        // A popup over the pane hides the pane's selection.
        let covered = self
            .hits
            .popup
            .as_ref()
            .filter(|_| matches!(pane, TextPane::Logs | TextPane::Healthcheck))
            .map(|popup| popup.area);
        let first = self.first_row(pane);
        // Rows end where their text does, as terminals highlight a selection, rather than at the
        // pane's edge.
        let drawn = self.draw_rows(&selection.context, first..first + usize::from(area.height));
        for y in 0..area.height {
            let row = first + usize::from(y);
            let Some((from, to)) = selection.columns_in_row(row, area.width) else {
                continue;
            };
            let Some(text_end) = text_end(&drawn, y, area.width) else {
                continue;
            };
            for x in from..=to.min(text_end) {
                let position = Position::new(area.x + x, area.y + y);
                if covered.is_some_and(|popup| popup.contains(position)) {
                    continue;
                }
                if let Some(cell) = buf.cell_mut(position) {
                    cell.modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
