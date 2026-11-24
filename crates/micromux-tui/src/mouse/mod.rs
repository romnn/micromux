//! Mouse support: what the last frame drew where, and what a press, a drag, a release or a turn
//! of the wheel does there.
//!
//! The renderer records every place the mouse can act on in [`Hits`] while it draws, so a click
//! lands on exactly what the frame showed there.
//! Under a popup only the popup reacts, and a press outside it closes it.
//!
//! Text is selected inside one pane at a time, never across panes: a drag selects from where it
//! started, a double click a word and a triple click a line.
//! A service's name in the list and a URL in the Logs pane's title select like a pane of their
//! own, so a selection never takes in a status, a marker or the title around a URL.
//! Selecting only highlights; `y` or Enter copies, or ending the drag when copy-on-select is
//! on.
//! A drag past the top or bottom of a pane scrolls it a row at a time.
//!
//! Hints and URLs act on a click, a press and a release on the same one, so a press that slides
//! off does nothing.
//! A right-click copies the URL it presses and releases on, and the selection anywhere else.

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
    /// The services the list shows, top to bottom, with their rows and names.
    pub(crate) service_names: Vec<NameHit>,
    /// The logs pane, borders and scrollbar included.
    pub(crate) logs: Rect,
    /// The side pane, or an empty area while it is hidden.
    pub(crate) side: Rect,
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

/// A service in the services list: the cells its name was drawn in, and all its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NameHit {
    pub(crate) area: Rect,
    /// The rows the service takes, two for one that names a resource.
    pub(crate) item: Rect,
    /// The service's ID.
    pub(crate) service: String,
    /// The name as the row shows it, which may cut it short.
    pub(crate) line: Line<'static>,
}

/// A URL and the cells it was drawn in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkHit {
    pub(crate) area: Rect,
    pub(crate) url: String,
    /// The URL as a line of text to select in.
    pub(crate) line: Line<'static>,
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

/// A press of the right button on a URL.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RightPress {
    /// The URL, which a release on it copies.
    Link(String),
    /// A URL the pointer slid off, so the release copies nothing, not even the selection.
    SlidOff,
}

/// What the footer says about the last thing the mouse did, for a few seconds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Notice {
    /// What a copy took, shown where the footer's last row has room beside its hints; where it
    /// has none, the footer's copy or mouse hint says that a URL or the selection was copied.
    Copied { text: String, url: bool },
    /// That `url` could not be opened, which covers the hints of the footer's last row when it
    /// has no room beside them, and says what to do instead as the frame it is drawn in allows.
    Failed { url: String },
}

impl Notice {
    /// How long the notice stays.
    pub(crate) fn duration(&self) -> Duration {
        match self {
            Self::Copied { .. } => Duration::from_secs(3),
            Self::Failed { .. } => Duration::from_secs(4),
        }
    }
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
    /// Whether a press and a drag select text, and a right click copies it or a URL.
    select: bool,
    /// Whether a click focuses, selects, presses hints, opens URLs and picks levels.
    navigate: bool,
    /// Whether the wheel scrolls.
    scroll: bool,
    /// Whether ending a drag copies the selection.
    copy_on_select: bool,
    pub(crate) selection: Option<Selection>,
    /// Whether the left button is down on a selection being made.
    dragging: bool,
    /// The hint or URL the left button went down on, which a release on it clicks.
    pressed: Option<Clickable>,
    /// The URL the first click of the current quick series of presses opened.
    opened: Option<String>,
    /// The URL the right button went down on, or `None` when it went down elsewhere.
    right_pressed: Option<RightPress>,
    last_press: Option<Press>,
    /// When the wheel event that last scrolled arrived, to fold the burst of events some
    /// terminals send for one notch into one step.
    last_wheel: Option<Instant>,
    /// What the footer says about the last thing the mouse did, until the next press or key.
    ///
    /// Changes go through [`MouseState::set_notice`], which counts them.
    pub(crate) notice: Option<Notice>,
    /// When the notice was given, from which it lasts its [`Notice::duration`].
    pub(crate) notice_at: Instant,
    /// How often the notice changed, to tell whether it did since a URL was clicked.
    notice_changes: u64,
}

impl MouseState {
    pub(crate) fn new(options: crate::MouseOptions) -> Self {
        let capture = options.select || options.navigate || options.scroll;
        // With every part off, capture starts off, and `m` turns it on with every part.
        let part = |enabled| enabled || !capture;
        Self {
            capture,
            select: part(options.select),
            navigate: part(options.navigate),
            scroll: part(options.scroll),
            copy_on_select: options.copy_on_select,
            selection: None,
            dragging: false,
            pressed: None,
            opened: None,
            right_pressed: None,
            last_press: None,
            last_wheel: None,
            notice: None,
            notice_at: Instant::now(),
            notice_changes: 0,
        }
    }

    /// Changes what the footer says about the last thing the mouse did, `now`.
    pub(crate) fn set_notice(&mut self, notice: Option<Notice>, now: Instant) {
        self.notice = notice;
        self.notice_at = now;
        self.notice_changes += 1;
    }

    /// When the notice goes, if there is one.
    pub(crate) fn notice_deadline(&self) -> Option<Instant> {
        self.notice
            .as_ref()
            .map(|notice| self.notice_at + notice.duration())
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
    /// The key a click on the side pane hint presses, which shows the side panes in turn: the
    /// healthcheck pane, then the events pane, then neither.
    fn next_side_pane_key(&self) -> KeyEvent {
        let ch = match self.side_pane {
            None => 'H',
            Some(crate::SidePane::Health | crate::SidePane::Events) => 'E',
        };
        KeyEvent {
            code: KeyCode::Char(ch),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Turns mouse capture on or off, as the `m` key does.
    pub(crate) fn toggle_mouse_capture(&mut self) {
        let capture = !self.mouse.capture;
        self.mouse.capture = capture;
        self.mouse.selection = None;
        self.mouse.dragging = false;
        self.mouse.pressed = None;
        self.mouse.right_pressed = None;
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
            // A right-button gesture that loses its press or release here ends, so the next one
            // never acts on what it pressed.
            if matches!(
                event.kind,
                MouseEventKind::Down(MouseButton::Right) | MouseEventKind::Up(MouseButton::Right)
            ) {
                self.mouse.right_pressed = None;
            }
            return;
        }
        let position = Position::new(event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.press(position, at),
            MouseEventKind::Drag(MouseButton::Left) => self.drag_to(position),
            MouseEventKind::Up(MouseButton::Left) => self.release(position),
            // A right-click copies the URL under it, or else the selection as `y` does, wherever
            // it is, and leaves the selection, the focus and the selected service as they are.
            // Every press and release of the right button updates what it went down on, even one
            // that copies nothing, so a gesture never acts on what an earlier one pressed.
            MouseEventKind::Down(MouseButton::Right) => {
                self.mouse.right_pressed = self
                    .link_at(position)
                    .filter(|_| self.right_click_copies())
                    .map(|url| RightPress::Link(url.to_string()));
            }
            MouseEventKind::Drag(MouseButton::Right) => {
                if let Some(RightPress::Link(url)) = &self.mouse.right_pressed
                    && self.link_at(position) != Some(url.as_str())
                {
                    self.mouse.right_pressed = Some(RightPress::SlidOff);
                }
            }
            MouseEventKind::Up(MouseButton::Right) => {
                let pressed = self.mouse.right_pressed.take();
                if self.right_click_copies() {
                    self.right_release(pressed, position);
                }
            }
            MouseEventKind::ScrollUp if self.mouse.scroll => self.wheel(position, false, at),
            MouseEventKind::ScrollDown if self.mouse.scroll => self.wheel(position, true, at),
            _ => {}
        }
    }

    /// Whether a right-click copies: selecting is on, keys go to the TUI rather than a service,
    /// and the level picker, which has nothing to copy, is closed.
    fn right_click_copies(&self) -> bool {
        self.mouse.select && !self.pty_input_mode && self.level_picker.is_none()
    }

    /// The URL drawn at `position`, if any; under a popup none reacts.
    fn link_at(&self, position: Position) -> Option<&str> {
        if self.hits.popup.is_some() {
            return None;
        }
        self.hits
            .links
            .iter()
            .find(|link| link.area.contains(position))
            .map(|link| link.url.as_str())
    }

    /// The right button, `pressed` on a URL or elsewhere, came up at `position`.
    ///
    /// A right-click on a URL copies it, and one that went down on a URL but slid off it copies
    /// nothing, so the clipboard only takes a URL pressed and released on.
    /// Any other right-click copies the selection.
    fn right_release(&mut self, pressed: Option<RightPress>, position: Position) {
        match pressed {
            Some(RightPress::Link(url)) => {
                if self.link_at(position) == Some(url.as_str()) {
                    let notice = Notice::Copied {
                        text: "copied the URL".to_string(),
                        url: true,
                    };
                    self.mouse.set_notice(Some(notice), Instant::now());
                    self.effects.push(Effect::Copy(url));
                }
            }
            Some(RightPress::SlidOff) => {}
            None => self.copy_selection(),
        }
    }

    /// The URL drawn at `position` as a pane to select in, if any.
    fn url_pane_at(&self, position: Position) -> Option<TextPane> {
        self.hits
            .links
            .iter()
            .position(|link| link.area.contains(position))
            .map(TextPane::Url)
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
        if count == 1 {
            self.mouse.opened = None;
        }
        self.mouse.set_notice(None, now);
        self.mouse.dragging = false;
        self.mouse.pressed = None;

        // A press closes the search prompt and does nothing else, as one outside a popup does,
        // so a click on a hint cannot type its key into the query.
        if self.search.prompt.take().is_some() {
            return;
        }

        // A hint or URL acts on release, and a hint keeps the selection, which the copy hint
        // copies.
        // Under a popup only the copy hint does, since copying text selected in the popup is the
        // one thing a click outside it may do without closing it.
        // Copying is part of selecting, so the copy hint, and the note that says a copy took the
        // hint's place, work without navigation, while a press on any other hint or URL then
        // counts as a press on whatever lies under it.
        let copying = |clickable: &Clickable| matches!(clickable, Clickable::Hint("y" | ""));
        if let Some(clickable) = self.clickable_at(position)
            && (self.hits.popup.is_none() || clickable == Clickable::Hint("y"))
            && (self.mouse.navigate || (self.mouse.select && copying(&clickable)))
        {
            // Once a click of a quick series opened the URL, the presses after it only select.
            let opened = match &clickable {
                Clickable::Link(url) => self.mouse.opened.as_ref() == Some(url),
                Clickable::Hint(_) => false,
            };
            if !(opened && self.mouse.select) {
                self.mouse.pressed = Some(clickable);
            }
            // A press on a URL also starts a selection in it, so a drag along the URL selects it
            // instead of opening it.
            if self.mouse.select
                && let Some(pane) = self.url_pane_at(position)
            {
                self.start_selection(pane, position, count);
            }
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
            // A press outside closes the popup whatever the mouse may do, so the popup cannot trap
            // the pointer.
            match target {
                PopupPress::Outside => self.close_popup(),
                PopupPress::Level(index) if self.mouse.navigate => self.pick_level(index),
                PopupPress::Text(pane) if self.mouse.select => {
                    self.start_selection(pane, position, count);
                }
                PopupPress::Level(_) | PopupPress::Text(_) | PopupPress::Inert => {}
            }
            return;
        }

        if self.hits.services.contains(position) {
            if self.mouse.navigate {
                self.focus = Focus::Services;
                // Typed input goes to the selected service, so a click must not retarget it.
                if !self.pty_input_mode
                    && let Some(hit) = self
                        .hits
                        .service_names
                        .iter()
                        .find(|hit| hit.item.contains(position))
                    && let Some(index) = self
                        .state
                        .services
                        .iter()
                        .position(|service| service.snapshot.id == hit.service)
                {
                    self.state.selected_service = index;
                }
            }
            // Of a service's row only its name selects.
            if self.mouse.select
                && let Some(index) = self
                    .hits
                    .service_names
                    .iter()
                    .position(|name| name.area.contains(position))
            {
                self.start_selection(TextPane::ServiceName(index), position, count);
            }
        } else if self.hits.logs.contains(position) {
            if self.mouse.navigate {
                self.focus = Focus::Logs;
            }
            // Of the title only its URLs select.
            if self.mouse.select {
                let pane = self.url_pane_at(position).unwrap_or(TextPane::Logs);
                self.start_selection(pane, position, count);
            }
        } else if self.hits.side.contains(position) {
            if self.mouse.navigate {
                self.focus = Focus::Side;
            }
            if self.mouse.select
                && let Some(pane) = self.side_text_pane()
            {
                self.start_selection(pane, position, count);
            }
        }
    }

    /// Clicks `clickable`: a hint runs its key, as if it were pressed, and a URL opens.
    fn click(&mut self, clickable: Clickable) {
        match clickable {
            Clickable::Hint(keys) => {
                let key = if keys == crate::render::SIDE_PANE_KEYS {
                    Some(self.next_side_pane_key())
                } else {
                    hint_key(keys)
                };
                let Some(key) = key else {
                    return;
                };
                // In PTY input mode keys go to the service, so only the hint that leaves the mode
                // acts.
                if self.pty_input_mode && keys != "Ctrl+]" {
                    return;
                }
                self.handle_key_press(key);
            }
            Clickable::Link(url) => {
                self.mouse.opened = Some(url.clone());
                self.effects.push(Effect::OpenUrl {
                    url,
                    notice_changes: self.mouse.notice_changes,
                });
            }
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
        // Sliding off a hint or URL cancels its click, and so does a drag that selects along a
        // URL.
        let moved = self
            .mouse
            .last_press
            .is_some_and(|press| press.position != position);
        if self.mouse.pressed.as_ref().is_some_and(|pressed| {
            self.clickable_at(position).as_ref() != Some(pressed)
                || (matches!(pressed, Clickable::Link(_)) && self.mouse.dragging && moved)
        }) {
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
        let dragging = std::mem::take(&mut self.mouse.dragging);
        if let Some(pressed) = self.mouse.pressed.take()
            && self.clickable_at(position).as_ref() == Some(&pressed)
        {
            // The caret the press on a URL placed goes with the click, so `Esc` goes on to clear
            // the search.
            if matches!(pressed, Clickable::Link(_)) && !self.has_selection() {
                self.mouse.selection = None;
            }
            self.click(pressed);
            return;
        }
        if dragging && self.mouse.copy_on_select {
            self.copy_selection();
        }
    }

    /// Copies the selection to the clipboard, as `y` does, and says so in the footer.
    pub(crate) fn copy_selection(&mut self) {
        let Some(text) = self.selected_text() else {
            return;
        };
        let lines = text.lines().count().max(1);
        let text_notice = if lines == 1 {
            format!("copied {} characters", text.chars().count())
        } else {
            format!("copied {lines} lines")
        };
        let notice = Notice::Copied {
            text: text_notice,
            url: false,
        };
        self.mouse.set_notice(Some(notice), Instant::now());
        self.effects.push(Effect::Copy(text));
    }

    /// Says in the footer, from `now`, that `url`, clicked when the notice had changed
    /// `notice_changes` times, could not be opened.
    ///
    /// The failure is known only once the input around the click is all handled, so a press or
    /// key since then, which changed the notice, keeps it from saying so.
    pub(crate) fn url_open_failed(&mut self, url: &str, notice_changes: u64, now: Instant) {
        if notice_changes != self.mouse.notice_changes {
            return;
        }
        let notice = Notice::Failed {
            url: url.to_string(),
        };
        self.mouse.set_notice(Some(notice), now);
    }

    /// What the footer says of `url`, which could not be opened, in a frame whose Logs title
    /// shows `links`: that a right-click on it copies it, while one would.
    pub(crate) fn open_failure_note(&self, url: &str, links: &[LinkHit]) -> &'static str {
        let popup = self.show_help || self.info.is_some() || self.level_picker.is_some();
        let copies = links.iter().any(|link| link.url == url)
            && self.mouse.capture
            && self.right_click_copies()
            && !popup;
        if copies {
            "cannot open the URL; right-click it to copy it"
        } else {
            "cannot open the URL"
        }
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
        } else if self.hits.side.contains(position)
            && let Some(pane) = self.side_text_pane()
        {
            self.scroll_pane(pane, down, WHEEL_ROWS);
        }
    }

    /// Scrolls `pane` by `rows`, as the movement keys do.
    fn scroll_pane(&mut self, pane: TextPane, down: bool, rows: u16) {
        match (pane, down) {
            (TextPane::Logs, false) => self.scroll_logs_up(rows),
            (TextPane::Logs, true) => self.scroll_logs_down(rows),
            (TextPane::Healthcheck | TextPane::Events, false) => self.scroll_side_pane_up(rows),
            (TextPane::Healthcheck | TextPane::Events, true) => self.scroll_side_pane_down(rows),
            // A service's name and a URL are one row each.
            (TextPane::ServiceName(_) | TextPane::Url(_), _) => {}
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
