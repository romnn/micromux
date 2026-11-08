//! Mouse support: what the last frame drew where, and what a press, a drag, a release or a turn
//! of the wheel does there.
//!
//! The renderer records every place the mouse can act on in [`Hits`] while it draws, so a click
//! lands on exactly what the frame showed there.
//! Under a popup only the popup reacts, and a press outside it closes it.
//!
//! Text is selected inside one pane at a time, never across panes: a drag selects from where it
//! started, a double click a word and a triple click a line.
//! Selecting only highlights; `y` or Enter copies, or ending the drag when copy-on-select is
//! on.
//! A drag past the top or bottom of a pane scrolls it a row at a time.
//!
//! Hints and URLs act on a click, a press and a release on the same one, so a press that slides
//! off does nothing.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;

use crate::{App, Effect, Focus};

mod select;

pub(crate) use select::{END_OF_LINE, Selection, TextPane, TextPos};

/// Rows one turn of the wheel scrolls a pane, as terminals scroll their own.
const WHEEL_ROWS: u16 = 3;

/// The shortest time between the arrivals of two steps of the wheel.
///
/// Some terminals report one notch as a burst of events written together, which arrive within
/// microseconds and make one step.
/// Separate notches, even of a fast spin, come further apart, and each makes a step; a
/// high-resolution or smooth-scrolling device's stream makes a step at most this often.
const WHEEL_STEP: Duration = Duration::from_millis(15);

/// How soon a second press on the same cell counts as a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

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

/// What a press inside or outside the popup lands on.
enum PopupPress {
    Outside,
    Level(usize),
    Text(TextPane),
    /// The popup's border or padding.
    Inert,
}

/// Something a click acts on, once the button comes up on it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Clickable {
    Hint(&'static str),
    Link(String),
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
    /// Whether ending a drag copies the selection.
    copy_on_select: bool,
    pub(crate) selection: Option<Selection>,
    /// Whether the left button is down on a selection being made.
    dragging: bool,
    /// The hint or URL the left button went down on, which a release on it clicks.
    pressed: Option<Clickable>,
    last_press: Option<Press>,
    /// When the wheel event that last scrolled arrived, to fold the burst of events some
    /// terminals send for one notch into one step.
    last_wheel: Option<Instant>,
    /// What the last copy took, shown in the footer until the next press or key.
    pub(crate) notice: Option<String>,
}

impl MouseState {
    pub(crate) fn new(options: crate::MouseOptions) -> Self {
        Self {
            capture: options.capture,
            copy_on_select: options.copy_on_select,
            selection: None,
            dragging: false,
            pressed: None,
            last_press: None,
            last_wheel: None,
            notice: None,
        }
    }

    /// Whether a wheel event that arrived `at` scrolls, rather than belonging to the burst of
    /// events whose first one already took the step.
    fn wheel_steps(&mut self, at: Instant) -> bool {
        if self
            .last_wheel
            .is_some_and(|last| at.saturating_duration_since(last) < WHEEL_STEP)
        {
            return false;
        }
        self.last_wheel = Some(at);
        true
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
        "Ctrl+]" => Some(key(KeyCode::Char(']'), KeyModifiers::CONTROL)),
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
        self.mouse.pressed = None;
        self.effects.push(Effect::MouseCapture(capture));
    }

    /// Acts on a mouse event from the terminal that arrived `at`.
    pub(crate) fn handle_mouse(&mut self, event: MouseEvent, at: Instant) {
        // The terminal may still report events it sent before capture went off.
        if !self.mouse.capture {
            return;
        }
        // A key or an earlier click in the same batch of input opened or closed a popup that the
        // last frame does not show yet, so the frame's areas no longer say what is where.
        let popup_open = self.show_help || self.info.is_some() || self.level_picker.is_some();
        if popup_open != self.hits.popup.is_some() {
            return;
        }
        let position = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.press(position, at),
            MouseEventKind::Drag(MouseButton::Left) => self.drag_to(position),
            MouseEventKind::Up(MouseButton::Left) => self.release(position),
            // A right-click copies the selection as `y` does, wherever it is, and leaves it, the
            // focus and the selected service as they are.
            MouseEventKind::Up(MouseButton::Right)
                if !self.pty_input_mode && self.level_picker.is_none() =>
            {
                self.copy_selection();
            }
            MouseEventKind::ScrollUp => self.wheel(position, false, at),
            MouseEventKind::ScrollDown => self.wheel(position, true, at),
            _ => {}
        }
    }

    /// The hint or URL drawn at `position`, if any.
    fn clickable_at(&self, position: Position) -> Option<Clickable> {
        let hint = self
            .hits
            .hints
            .iter()
            .find(|hint| hint.area.contains(position))
            .map(|hint| Clickable::Hint(hint.keys));
        hint.or_else(|| {
            self.hits
                .links
                .iter()
                .find(|link| link.area.contains(position))
                .map(|link| Clickable::Link(link.url.clone()))
        })
    }

    /// A press of the left button at `position`, `now`.
    fn press(&mut self, position: Position, now: Instant) {
        let count = self.mouse.count_press(position, now);
        self.mouse.notice = None;
        self.mouse.dragging = false;
        self.mouse.pressed = None;

        // A press closes the search prompt and does nothing else, as one outside a popup does,
        // so a click on a hint cannot type its key into the query.
        if self.search.prompt.take().is_some() {
            return;
        }

        // A hint or URL acts on release and keeps the selection, which the copy hint copies.
        // Under a popup only the copy hint does, since copying text selected in the popup is the
        // one thing a click outside it may do without closing it.
        if let Some(clickable) = self.clickable_at(position)
            && (self.hits.popup.is_none() || clickable == Clickable::Hint("y"))
        {
            self.mouse.pressed = Some(clickable);
            return;
        }
        self.mouse.selection = None;

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

    /// Clicks `clickable`: a hint runs its key, as if it were pressed, and a URL opens.
    fn click(&mut self, clickable: Clickable) {
        match clickable {
            Clickable::Hint(keys) => {
                let Some(key) = hint_key(keys) else {
                    return;
                };
                // In PTY input mode keys go to the service, so only the hint that leaves the mode
                // acts.
                if self.pty_input_mode && keys != "Ctrl+]" {
                    return;
                }
                self.handle_key_press(key);
            }
            Clickable::Link(url) => self.effects.push(Effect::OpenUrl(url)),
        }
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
        let Some(last_row) = self.total_rows(pane).checked_sub(1) else {
            return;
        };
        let row = self.first_row(pane) + usize::from(position.y - area.y);
        // Below the text, the selection starts at the end of the last line, so lines that come
        // later never join it.
        let pos = if row > last_row {
            TextPos {
                row: last_row,
                col: END_OF_LINE,
            }
        } else {
            TextPos {
                row,
                col: position.x - area.x,
            }
        };
        let (anchor, head) = match count {
            1 => (pos, None),
            2 => self
                .word_at(&context, pos)
                .map_or((pos, None), |(from, to)| (from, Some(to))),
            _ => {
                let (from, to) = self.line_at(&context, pos);
                (from, Some(to))
            }
        };
        self.select(context, anchor, head);
        self.mouse.dragging = true;
    }

    /// The left button moved to `position` while down: the selection extends there, and past the
    /// pane's top or bottom the pane scrolls a row.
    fn drag_to(&mut self, position: Position) {
        // Sliding off a hint or URL cancels its click.
        if self
            .mouse
            .pressed
            .as_ref()
            .is_some_and(|pressed| self.clickable_at(position).as_ref() != Some(pressed))
        {
            self.mouse.pressed = None;
        }
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
        let Some(area) = self.text_area(pane).filter(|area| !area.is_empty()) else {
            return;
        };
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
        let row = self.first_row(pane) + usize::from(edge_row);
        // Past the pane's right edge, the selection runs to the end of the line, even where an
        // unwrapped line goes on beyond the edge, and below the text, to the end of the last line.
        let col = if position.x >= area.right() || row > last_row {
            END_OF_LINE
        } else {
            position.x.saturating_sub(area.x)
        };
        self.extend_selection(TextPos {
            row: row.min(last_row),
            col,
        });
    }

    /// The left button came up at `position`: a click on a hint or URL acts, and a selection
    /// being made ends, copied only when copy-on-select is on.
    fn release(&mut self, position: Position) {
        if let Some(pressed) = self.mouse.pressed.take()
            && self.clickable_at(position).as_ref() == Some(&pressed)
        {
            self.click(pressed);
            return;
        }
        if std::mem::take(&mut self.mouse.dragging) && self.mouse.copy_on_select {
            self.copy_selection();
        }
    }

    /// Copies the selection to the clipboard, as `y` does, and says so in the footer.
    pub(crate) fn copy_selection(&mut self) {
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

    /// A turn of the wheel at `position` that arrived `at`: under a popup it scrolls the popup,
    /// elsewhere the pane under the pointer, without moving the focus; the services list moves a
    /// service a step, the text panes three rows.
    fn wheel(&mut self, position: Position, down: bool, at: Instant) {
        if !self.mouse.wheel_steps(at) {
            return;
        }
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
            // Typed input goes to the selected service, so the wheel must not retarget it.
            if self.pty_input_mode {
                return;
            }
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
}

#[cfg(test)]
mod tests;
