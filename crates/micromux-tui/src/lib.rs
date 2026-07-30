//! `micromux-tui` provides the terminal user interface for micromux.
//!
//! The main entry point is [`App`], constructed from a [`SessionSource`], an optional PTY-input
//! sender, a shutdown token, and display options; call [`App::render`] to run it. The TUI holds only
//! view state and re-reads its synchronous source on each [`micromux::SessionChange`].

mod event;
mod json_log;
mod remote;
mod render;
mod source;
mod state;
mod style;

use std::collections::VecDeque;
use std::time::Duration;

use micromux::{ChangeKind, Command, SessionChange};
use ratatui::DefaultTerminal;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

pub use remote::RemoteSource;
pub use source::{LocalSource, SessionSource};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const PENDING_PTY_INPUT_MAX_BYTES: usize = 64 * 1024;
const PENDING_PTY_INPUT_RETRY: Duration = Duration::from_millis(5);

/// Errors from running or rendering the terminal interface.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The terminal event task stopped before the application shut down.
    #[error("terminal input event channel closed")]
    InputClosed,
    /// The terminal backend failed.
    #[error(transparent)]
    Terminal(#[from] std::io::Error),
}

/// Terminal application state.
pub struct App {
    /// Running state of the TUI application.
    running: bool,
    input: Option<mpsc::Sender<Command>>,
    pending_pty_input: VecDeque<(String, Vec<u8>)>,
    pending_pty_input_bytes: usize,
    shutdown: micromux::CancellationToken,
    /// Synchronous session data and lifecycle commands.
    source: SessionSource,
    /// Liveness-only change notifications; the TUI re-queries the model for content.
    changes: broadcast::Receiver<SessionChange>,
    /// Event handler
    input_event_handler: event::InputHandler,
    /// Current state
    state: state::State,
    /// Log viewer
    log_view: crate::render::log_view::LogView,
    healthcheck_view: crate::render::log_view::LogView,
    show_healthcheck_pane: bool,
    pretty_json_logs: bool,
    pty_input_mode: bool,
    focus: Focus,
    terminal_cols: u16,
    terminal_rows: u16,
    last_pty_cols: u16,
    last_pty_rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// Service list is focused.
    Services,
    /// Log pane is focused.
    Logs,
    /// Healthcheck pane is focused.
    Healthcheck,
}

impl App {
    /// Construct a new [`App`] over a synchronous session source.
    #[must_use]
    pub fn new(
        source: SessionSource,
        input: Option<mpsc::Sender<Command>>,
        shutdown: micromux::CancellationToken,
        pretty_json_logs: bool,
    ) -> Self {
        let changes = source.subscribe();
        let snapshots = source.services();

        let services = snapshots.into_iter().map(state::Service::new).collect();

        let log_view = render::log_view::LogView::default();
        let healthcheck_view = render::log_view::LogView::default();

        Self {
            running: true,
            input,
            pending_pty_input: VecDeque::new(),
            pending_pty_input_bytes: 0,
            shutdown,
            source,
            changes,
            input_event_handler: event::InputHandler::new(),
            state: state::State::new(services),
            log_view,
            healthcheck_view,
            show_healthcheck_pane: false,
            pretty_json_logs,
            pty_input_mode: false,
            focus: Focus::Services,
            terminal_cols: 80,
            terminal_rows: 24,
            last_pty_cols: 0,
            last_pty_rows: 0,
        }
    }
}

impl App {
    fn desired_pty_size(&self) -> (u16, u16) {
        use ratatui::layout::{Constraint, Direction, Layout, Rect};

        let area = Rect {
            x: 0,
            y: 0,
            width: self.terminal_cols,
            height: self.terminal_rows,
        };

        let [_header_area, main_area, _footer_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // header (must match render's vertical layout)
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .spacing(0)
            .areas(area);

        let [_services_area, main_right_area] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(self.state.services_sidebar_width),
                Constraint::Min(0),
            ])
            .spacing(0)
            .areas(main_area);

        let logs_area = if self.show_healthcheck_pane {
            let [a, _b] = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .spacing(0)
                .areas::<2>(main_right_area);
            a
        } else {
            main_right_area
        };

        let [logs_pane_area, _scrollbar_area] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .spacing(0)
            .areas(logs_area);

        let cols = logs_pane_area.width.saturating_sub(2).max(1);
        let rows = logs_pane_area.height.saturating_sub(2).max(1);
        (cols, rows)
    }

    fn maybe_resize_pty(&mut self) {
        let Some(input) = &self.input else {
            return;
        };
        let (cols, rows) = self.desired_pty_size();
        if cols == self.last_pty_cols && rows == self.last_pty_rows {
            return;
        }

        // Only advance the dedup cache once the resize is actually queued; otherwise a dropped
        // ResizeAll (full channel) would leave the cache claiming the new size and never retry.
        if input.try_send(Command::ResizeAll { cols, rows }).is_ok() {
            self.last_pty_cols = cols;
            self.last_pty_rows = rows;
        }
    }

    fn enqueue_pty_input(&mut self, service_id: String, bytes: Vec<u8>) {
        let Some(input) = self.input.clone() else {
            return;
        };
        if self.pending_pty_input.is_empty() {
            match input.try_send(Command::SendInput(service_id, bytes)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(Command::SendInput(service_id, bytes))) => {
                    self.push_pending_pty_input(service_id, bytes);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    tracing::warn!("PTY input channel closed; discarding terminal input");
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!("unexpected command returned from PTY input queue");
                }
            }
        } else {
            self.push_pending_pty_input(service_id, bytes);
        }
    }

    fn push_pending_pty_input(&mut self, service_id: String, bytes: Vec<u8>) {
        if self.pending_pty_input_bytes.saturating_add(bytes.len()) > PENDING_PTY_INPUT_MAX_BYTES {
            tracing::warn!(
                queue_limit_bytes = PENDING_PTY_INPUT_MAX_BYTES,
                "PTY input backlog is full; discarding terminal input"
            );
            return;
        }
        self.pending_pty_input_bytes = self.pending_pty_input_bytes.saturating_add(bytes.len());
        if let Some((queued_service, queued)) = self.pending_pty_input.back_mut()
            && queued_service == &service_id
        {
            queued.extend(bytes);
        } else {
            self.pending_pty_input.push_back((service_id, bytes));
        }
    }

    fn flush_pending_pty_input(&mut self) {
        let Some(input) = &self.input else {
            self.pending_pty_input.clear();
            self.pending_pty_input_bytes = 0;
            return;
        };
        while let Some((service_id, bytes)) = self.pending_pty_input.pop_front() {
            let bytes_len = bytes.len();
            match input.try_send(Command::SendInput(service_id, bytes)) {
                Ok(()) => {
                    self.pending_pty_input_bytes =
                        self.pending_pty_input_bytes.saturating_sub(bytes_len);
                }
                Err(mpsc::error::TrySendError::Full(Command::SendInput(service_id, bytes))) => {
                    self.pending_pty_input.push_front((service_id, bytes));
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    tracing::warn!("PTY input channel closed; discarding queued terminal input");
                    self.pending_pty_input.clear();
                    self.pending_pty_input_bytes = 0;
                    break;
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!("unexpected command returned from PTY input queue");
                }
            }
        }
    }

    /// Run the TUI event loop.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Receiving an input event fails.
    /// - The underlying terminal backend fails to draw.
    pub async fn run(mut self, mut terminal: DefaultTerminal) -> Result<(), Error> {
        enum Wake {
            Input(event::Input),
            Change(SessionChange),
            /// The change broadcast lagged (or this is the initial draw): re-read everything.
            Resync,
            PendingInput,
        }

        let area = terminal.size()?;
        self.terminal_cols = area.width;
        self.terminal_rows = area.height;
        self.maybe_resize_pty();

        // Seed the view from the model before the first frame.
        self.resync();
        terminal.draw(|frame| frame.render_widget(&mut self, frame.area()))?;
        let mut last_frame = tokio::time::Instant::now();

        while self.is_running() {
            let retry_pending_input = !self.pending_pty_input.is_empty();
            let wake = tokio::select! {
                () = self.shutdown.cancelled() => None,
                input = self.input_event_handler.next() => {
                    Some(Wake::Input(input.ok_or(Error::InputClosed)?))
                },
                change = self.changes.recv() => match change {
                    Ok(change) => Some(Wake::Change(change)),
                    Err(broadcast::error::RecvError::Lagged(_)) => Some(Wake::Resync),
                    Err(broadcast::error::RecvError::Closed) => None,
                },
                () = async {
                    if retry_pending_input {
                        tokio::time::sleep(PENDING_PTY_INPUT_RETRY).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => Some(Wake::PendingInput),
            };

            match wake {
                Some(Wake::Input(event)) => self.handle_input_event(event),
                Some(Wake::Change(change)) => self.apply_change(&change),
                Some(Wake::Resync) => self.resync(),
                Some(Wake::PendingInput) => self.flush_pending_pty_input(),
                None => {
                    self.running = false;
                    continue;
                }
            }
            while self.running {
                let Some(event) = self.input_event_handler.try_next() else {
                    break;
                };
                self.handle_input_event(event);
            }
            loop {
                match self.changes.try_recv() {
                    Ok(change) => self.apply_change(&change),
                    Err(broadcast::error::TryRecvError::Lagged(_)) => self.resync(),
                    Err(
                        broadcast::error::TryRecvError::Empty
                        | broadcast::error::TryRecvError::Closed,
                    ) => break,
                }
            }
            let frame_deadline = last_frame + FRAME_INTERVAL;
            if tokio::time::Instant::now() < frame_deadline {
                tokio::time::sleep_until(frame_deadline).await;
            }
            terminal.draw(|frame| frame.render_widget(&mut self, frame.area()))?;
            last_frame = tokio::time::Instant::now();
        }
        Ok(())
    }

    /// Apply a single liveness notification by re-querying the model for the affected service. The
    /// broadcast carries only `{service_id, kind}`; the content lives in the model.
    fn apply_change(&mut self, change: &SessionChange) {
        match change.kind {
            ChangeKind::Status => {
                if let Some(snapshot) = self.source.service(&change.service_id)
                    && let Some(service) = self.service_mut(&change.service_id)
                {
                    service.snapshot = snapshot;
                }
            }
            ChangeKind::Logs => {
                if let Some(service) = self.service_mut(&change.service_id) {
                    service.logs_dirty = true;
                }
            }
            ChangeKind::Health => {
                if let Some(service) = self.service_mut(&change.service_id) {
                    service.healthcheck_dirty = true;
                }
            }
            ChangeKind::Roster | ChangeKind::Unknown => self.resync(),
            ChangeKind::Events => {}
        }
    }

    /// Reconcile the roster while retaining render caches for services that still exist.
    fn resync(&mut self) {
        let selected_id = self
            .state
            .current_service()
            .map(|service| service.snapshot.id.clone());
        let previous_index = self.state.selected_service;
        let mut existing = std::mem::take(&mut self.state.services)
            .into_iter()
            .map(|service| (service.snapshot.id.clone(), service))
            .collect::<std::collections::HashMap<_, _>>();
        self.state.services = self
            .source
            .services()
            .into_iter()
            .map(|snapshot| {
                if let Some(mut service) = existing.remove(&snapshot.id) {
                    service.snapshot = snapshot;
                    service.logs_dirty = true;
                    service.healthcheck_dirty = true;
                    service
                } else {
                    state::Service::new(snapshot)
                }
            })
            .collect();
        self.state.selected_service = selected_id
            .and_then(|selected_id| {
                self.state
                    .services
                    .iter()
                    .position(|service| service.snapshot.id == selected_id)
            })
            .unwrap_or_else(|| previous_index.min(self.state.services.len().saturating_sub(1)));
    }

    fn service_mut(&mut self, id: &str) -> Option<&mut state::Service> {
        self.state
            .services
            .iter_mut()
            .find(|service| service.snapshot.id == id)
    }

    fn handle_input_event(&mut self, input_event: event::Input) {
        let event::Input::Event(event) = input_event;
        self.handle_crossterm_event(&event);
    }

    fn handle_crossterm_event(&mut self, event: &crossterm::event::Event) {
        use crossterm::event::KeyEventKind;

        match *event {
            crossterm::event::Event::Resize(cols, rows) => {
                self.terminal_cols = cols;
                self.terminal_rows = rows;
                self.maybe_resize_pty();
            }
            crossterm::event::Event::Key(key) if key.kind == KeyEventKind::Press => {
                self.handle_key_press(key);
            }
            crossterm::event::Event::Paste(ref text) if self.pty_input_mode => {
                if let Some(service) = self.state.current_service() {
                    let service_id = service.snapshot.id.clone();
                    self.enqueue_pty_input(service_id, text.as_bytes().to_vec());
                }
            }
            _ => {}
        }
    }

    fn handle_key_press(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;

        if self.pty_input_mode {
            self.handle_key_press_pty_input_mode(key);
            return;
        }

        match key.code {
            // Quit
            KeyCode::Char('q') => self.exit(),

            // Toggle focus
            KeyCode::Tab => self.toggle_focus(),

            // Toggle PTY input mode
            KeyCode::Char('a') if self.input.is_some() => {
                self.pty_input_mode = !self.pty_input_mode;
            }
            KeyCode::Char('H') => self.toggle_healthcheck_pane(),

            // Disable current service
            KeyCode::Char('d') => self.disable_current_service(),

            KeyCode::Char('s') => self.stop_current_dynamic_service(),

            // Restart service
            KeyCode::Char('r') => self.restart_current_service(),

            // Restart all services
            KeyCode::Char('R') => self.restart_all_services(),

            // Navigation
            KeyCode::Char('k') | KeyCode::Up => self.navigate_up(),
            KeyCode::Char('j') | KeyCode::Down => self.navigate_down(),
            KeyCode::Char('g') => self.scroll_to_top(),
            KeyCode::Char('G') => self.scroll_to_bottom(),

            // Decrease service sidebar width (resize to the left)
            KeyCode::Char('-' | 'h') | KeyCode::Left => {
                self.state.resize_left();
                self.maybe_resize_pty();
            }

            // Increase service sidebar width (resize to the right)
            KeyCode::Char('+' | 'l') | KeyCode::Right => {
                self.state.resize_right();
                self.maybe_resize_pty();
            }

            // Toggle wrapping for log viewer
            KeyCode::Char('w') => self.toggle_wrap(),

            // Toggle automatic tailing for log viewer
            KeyCode::Char('t') => self.toggle_tail(),
            _ => {}
        }
    }

    fn handle_key_press_pty_input_mode(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        match key.code {
            KeyCode::Esc if key.modifiers.contains(KeyModifiers::ALT) => {
                self.pty_input_mode = false;
            }
            _ => {
                if let Some(bytes) = key_event_to_bytes(key.code, key.modifiers)
                    && let Some(service) = self.state.current_service()
                {
                    let service_id = service.snapshot.id.clone();
                    self.enqueue_pty_input(service_id, bytes);
                }
            }
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = if self.show_healthcheck_pane {
            match self.focus {
                Focus::Services => Focus::Logs,
                Focus::Logs => Focus::Healthcheck,
                Focus::Healthcheck => Focus::Services,
            }
        } else {
            match self.focus {
                Focus::Services => Focus::Logs,
                Focus::Logs | Focus::Healthcheck => Focus::Services,
            }
        };
    }

    fn toggle_healthcheck_pane(&mut self) {
        self.show_healthcheck_pane = !self.show_healthcheck_pane;
        if !self.show_healthcheck_pane && self.focus == Focus::Healthcheck {
            self.focus = Focus::Logs;
        }
        self.maybe_resize_pty();
    }

    fn navigate_up(&mut self) {
        match self.focus {
            Focus::Services => self.state.service_up(),
            Focus::Logs => self.scroll_logs_up(1),
            Focus::Healthcheck => self.scroll_healthchecks_up(1),
        }
    }

    fn navigate_down(&mut self) {
        match self.focus {
            Focus::Services => self.state.service_down(),
            Focus::Logs => self.scroll_logs_down(1),
            Focus::Healthcheck => self.scroll_healthchecks_down(1),
        }
    }

    fn scroll_to_top(&mut self) {
        match self.focus {
            Focus::Logs => {
                self.log_view.follow_tail = false;
                self.log_view.scroll_offset = 0;
            }
            Focus::Healthcheck => {
                self.healthcheck_view.follow_tail = false;
                self.healthcheck_view.scroll_offset = 0;
            }
            Focus::Services => {}
        }
    }

    fn scroll_to_bottom(&mut self) {
        match self.focus {
            Focus::Logs => {
                self.log_view.follow_tail = true;
            }
            Focus::Healthcheck => {
                self.healthcheck_view.follow_tail = true;
            }
            Focus::Services => {}
        }
    }

    fn toggle_wrap(&mut self) {
        let wrap = !self.log_view.wrap;
        self.log_view.wrap = wrap;
        self.healthcheck_view.wrap = wrap;
    }

    fn toggle_tail(&mut self) {
        match self.focus {
            Focus::Logs | Focus::Services => {
                self.log_view.follow_tail = !self.log_view.follow_tail;
            }
            Focus::Healthcheck => {
                self.healthcheck_view.follow_tail = !self.healthcheck_view.follow_tail;
            }
        }
    }

    fn log_viewport_height(&self) -> u16 {
        // total rows minus header (1) minus footer (1) minus logs block borders (2)
        self.terminal_rows.saturating_sub(4)
    }

    fn scroll_logs_up(&mut self, lines: u16) {
        self.log_view.follow_tail = false;
        self.log_view.scroll_offset = self.log_view.scroll_offset.saturating_sub(lines);
    }

    fn scroll_logs_down(&mut self, lines: u16) {
        self.log_view.follow_tail = false;
        let Some(service) = self.state.current_service() else {
            return;
        };
        // Wrap-aware count persisted by the renderer, so scrolling reaches the true bottom
        // even when wrapping is enabled (the raw entry count would stop short).
        let num_lines = service.cached_wrapped_lines;
        let viewport = self.log_viewport_height();
        let max_off = num_lines.saturating_sub(viewport);
        self.log_view.scroll_offset = self
            .log_view
            .scroll_offset
            .saturating_add(lines)
            .min(max_off);
    }

    fn scroll_healthchecks_up(&mut self, lines: u16) {
        self.healthcheck_view.follow_tail = false;
        self.healthcheck_view.scroll_offset =
            self.healthcheck_view.scroll_offset.saturating_sub(lines);
    }

    fn scroll_healthchecks_down(&mut self, lines: u16) {
        self.healthcheck_view.follow_tail = false;
        let Some(service) = self.state.current_service() else {
            return;
        };
        let num_lines = service.healthcheck_cached_num_lines;
        let viewport = self.log_viewport_height();
        let max_off = num_lines.saturating_sub(viewport);
        self.healthcheck_view.scroll_offset = self
            .healthcheck_view
            .scroll_offset
            .saturating_add(lines)
            .min(max_off);
    }

    fn is_running(&self) -> bool {
        self.running
    }

    fn exit(&mut self) {
        // A remote source owns only its mirror task, so cancelling it cannot stop the session.
        self.source.cancel();
        self.shutdown.cancel();
        self.running = false;
    }

    /// Disable service
    fn disable_current_service(&self) {
        let Some(service) = self.state.current_service() else {
            return;
        };
        tracing::info!(service_id = service.snapshot.id, "disabling service");
        match service.snapshot.desired {
            micromux::Desired::Disabled => self.source.enable(service.snapshot.id.clone()),
            micromux::Desired::Enabled => self.source.disable(service.snapshot.id.clone()),
            micromux::Desired::Unknown => {}
        }
    }

    fn stop_current_dynamic_service(&self) {
        let Some(service) = self.state.current_service() else {
            return;
        };
        if service.snapshot.origin != micromux::OriginKind::Dynamic
            || service.snapshot.retired.is_some()
        {
            return;
        }
        tracing::info!(service_id = service.snapshot.id, "retiring dynamic service");
        self.source.stop_dynamic(service.snapshot.id.clone());
    }

    /// Restart service
    fn restart_current_service(&self) {
        let Some(service) = self.state.current_service() else {
            return;
        };
        tracing::info!(service_id = service.snapshot.id, "restarting service");
        self.source.restart(service.snapshot.id.clone());
    }

    /// Restart all services
    fn restart_all_services(&self) {
        tracing::info!("restarting all services");
        self.source.restart_all();
    }
}

fn key_event_to_bytes(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Option<Vec<u8>> {
    use crossterm::event::KeyCode;

    if modifiers.contains(crossterm::event::KeyModifiers::ALT)
        && !modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
        && !matches!(code, KeyCode::Esc)
    {
        let base_modifiers = modifiers - crossterm::event::KeyModifiers::ALT;
        if let Some(mut bytes) = key_event_to_bytes(code, base_modifiers) {
            let mut out = Vec::with_capacity(1 + bytes.len());
            out.push(0x1b);
            out.append(&mut bytes);
            return Some(out);
        }
    }

    if modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char(c) => {
                let c = c.to_ascii_lowercase();
                if c.is_ascii_lowercase() {
                    return Some(vec![(c as u8) - b'a' + 1]);
                }
                match c {
                    '@' => return Some(vec![0x00]),
                    '[' => return Some(vec![0x1b]),
                    '\\' => return Some(vec![0x1c]),
                    ']' => return Some(vec![0x1d]),
                    '^' => return Some(vec![0x1e]),
                    '_' => return Some(vec![0x1f]),
                    _ => {}
                }
            }
            KeyCode::Enter => return Some(vec![b'\n']),
            _ => {}
        }
    }

    match code {
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Char(c) => Some(c.to_string().into_bytes()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codespan_reporting::diagnostic::Diagnostic;
    use color_eyre::eyre;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use indoc::indoc;
    use ratatui::widgets::Widget as _;
    use similar_asserts::assert_eq;
    use std::path::Path;
    use tokio::sync::mpsc;

    fn tab_press() -> crate::event::Input {
        crate::event::Input::Event(crossterm::event::Event::Key(KeyEvent {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }))
    }

    #[test]
    fn ctrl_letters_map_to_ascii_control_codes() {
        assert_eq!(
            key_event_to_bytes(KeyCode::Char('a'), KeyModifiers::CONTROL),
            Some(vec![0x01])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Char('z'), KeyModifiers::CONTROL),
            Some(vec![0x1a])
        );
    }

    #[test]
    fn ctrl_specials_map_to_standard_codes() {
        assert_eq!(
            key_event_to_bytes(KeyCode::Char('@'), KeyModifiers::CONTROL),
            Some(vec![0x00])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Char('['), KeyModifiers::CONTROL),
            Some(vec![0x1b])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Char('\\'), KeyModifiers::CONTROL),
            Some(vec![0x1c])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Char(']'), KeyModifiers::CONTROL),
            Some(vec![0x1d])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Char('^'), KeyModifiers::CONTROL),
            Some(vec![0x1e])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Char('_'), KeyModifiers::CONTROL),
            Some(vec![0x1f])
        );
    }

    #[test]
    fn special_keys_encode_as_expected() {
        assert_eq!(
            key_event_to_bytes(KeyCode::Esc, KeyModifiers::NONE),
            Some(vec![0x1b])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Enter, KeyModifiers::NONE),
            Some(vec![b'\r'])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Enter, KeyModifiers::CONTROL),
            Some(vec![b'\n'])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Tab, KeyModifiers::NONE),
            Some(vec![b'\t'])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::BackTab, KeyModifiers::NONE),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Backspace, KeyModifiers::NONE),
            Some(vec![0x7f])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Up, KeyModifiers::NONE),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Delete, KeyModifiers::NONE),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Home, KeyModifiers::NONE),
            Some(b"\x1b[H".to_vec())
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::End, KeyModifiers::NONE),
            Some(b"\x1b[F".to_vec())
        );
    }

    #[test]
    fn alt_char_is_esc_prefixed() {
        assert_eq!(
            key_event_to_bytes(KeyCode::Char('x'), KeyModifiers::ALT),
            Some(vec![0x1b, b'x'])
        );
    }

    #[tokio::test]
    async fn pty_input_waits_in_a_bounded_backlog_when_the_command_channel_is_full()
    -> eyre::Result<()> {
        let yaml = "version: 1\nservices:\n  svc:\n    command: [\"true\"]\n";
        let mut diagnostics: Vec<Diagnostic<usize>> = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!(err.to_string()))?;
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
        let (_runner, handles) = mux.start(micromux::CancellationToken::new());
        let (commands_tx, mut commands_rx) = mpsc::channel(1);
        commands_tx.try_send(Command::ResizeAll { cols: 1, rows: 1 })?;
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, commands_tx.clone())),
            Some(commands_tx),
            micromux::CancellationToken::new(),
            true,
        );

        app.enqueue_pty_input("svc".to_string(), b"first".to_vec());
        app.enqueue_pty_input("svc".to_string(), b"second".to_vec());
        assert_eq!(app.pending_pty_input_bytes, 11);
        let _ = commands_rx.try_recv()?;
        app.flush_pending_pty_input();

        match commands_rx.try_recv()? {
            Command::SendInput(service, bytes) => {
                assert_eq!(service, "svc");
                assert_eq!(bytes, b"firstsecond");
            }
            other => eyre::bail!("expected queued PTY input, got {other:?}"),
        }
        assert_eq!(app.pending_pty_input_bytes, 0);
        Ok(())
    }

    #[tokio::test]
    async fn bracketed_paste_is_inert_in_command_mode_and_atomic_in_input_mode() -> eyre::Result<()>
    {
        let yaml = "version: 1\nservices:\n  svc:\n    command: [\"true\"]\n";
        let mut diagnostics: Vec<Diagnostic<usize>> = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!(err.to_string()))?;
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
        let (_runner, handles) = mux.start(micromux::CancellationToken::new());
        let (commands_tx, mut commands_rx) = mpsc::channel(4);
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, commands_tx.clone())),
            Some(commands_tx),
            micromux::CancellationToken::new(),
            true,
        );

        app.handle_crossterm_event(&crossterm::event::Event::Paste("Rq".to_string()));
        assert!(app.running);
        assert!(matches!(
            commands_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        app.pty_input_mode = true;
        app.handle_crossterm_event(&crossterm::event::Event::Paste("echo pasted\n".to_string()));
        match commands_rx.try_recv()? {
            Command::SendInput(service, bytes) => {
                assert_eq!(service, "svc");
                assert_eq!(bytes, b"echo pasted\n");
            }
            other => eyre::bail!("expected one pasted PTY input batch, got {other:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn tab_cycles_focus_with_and_without_healthcheck_pane() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["sh", "-c", "true"]
        "#};
        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!(err.to_string()))?;
        let mux = std::sync::Arc::new(
            micromux::Micromux::new(&parsed).map_err(|err| eyre::eyre!(err.to_string()))?,
        );
        let shutdown = micromux::CancellationToken::new();
        // The runner is not spawned; the model reader is seeded with initial snapshots and is all
        // the focus test needs.
        let (_runner, handles) = mux.start(shutdown.clone());

        let mut app = App::new(
            SessionSource::Local(LocalSource::new(
                handles.reader.clone(),
                handles.commands.clone(),
            )),
            Some(handles.commands.clone()),
            shutdown,
            true,
        );
        app.focus = Focus::Services;
        app.show_healthcheck_pane = false;

        app.handle_input_event(tab_press());
        assert_eq!(app.focus, Focus::Logs);
        app.handle_input_event(tab_press());
        assert_eq!(app.focus, Focus::Services);

        app.show_healthcheck_pane = true;
        app.focus = Focus::Services;
        app.handle_input_event(tab_press());
        assert_eq!(app.focus, Focus::Logs);
        app.handle_input_event(tab_press());
        assert_eq!(app.focus, Focus::Healthcheck);
        app.handle_input_event(tab_press());
        assert_eq!(app.focus, Focus::Services);

        Ok(())
    }

    #[tokio::test]
    async fn pty_input_and_resize_keys_are_inert_without_an_input_sender() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["sh", "-c", "true"]
        "#};
        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!(err.to_string()))?;
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
        let shutdown = micromux::CancellationToken::new();
        let (_runner, handles) = mux.start(shutdown.clone());
        let (commands_tx, mut commands_rx) = mpsc::channel(4);
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, commands_tx)),
            None,
            shutdown,
            true,
        );
        let key = |code| KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        app.handle_key_press(key(KeyCode::Char('a')));
        assert!(!app.pty_input_mode);

        // Even an internally inconsistent state cannot route bytes without the capability.
        app.pty_input_mode = true;
        app.handle_key_press(key(KeyCode::Char('x')));
        app.handle_crossterm_event(&crossterm::event::Event::Resize(120, 40));

        assert!(matches!(
            commands_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        let area = ratatui::layout::Rect::new(0, 0, 160, 24);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        (&mut app).render(area, &mut buffer);
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(!rendered.contains("PTY Input"));
        assert!(!rendered.contains("Exit input"));
        Ok(())
    }

    #[tokio::test]
    async fn restart_key_on_disabled_service_does_not_send_enable() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["sh", "-c", "true"]
        "#};
        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!(err.to_string()))?;
        let mux = std::sync::Arc::new(
            micromux::Micromux::new(&parsed).map_err(|err| eyre::eyre!(err.to_string()))?,
        );
        let shutdown = micromux::CancellationToken::new();
        let (_runner, handles) = mux.start(shutdown.clone());
        let (commands_tx, mut commands_rx) = mpsc::channel(4);

        let mut app = App::new(
            SessionSource::Local(LocalSource::new(
                handles.reader.clone(),
                commands_tx.clone(),
            )),
            Some(commands_tx),
            shutdown,
            true,
        );
        if let Some(service) = app.state.current_service_mut() {
            service.snapshot.desired = micromux::Desired::Disabled;
        }

        app.restart_current_service();

        match commands_rx.try_recv()? {
            micromux::Command::Restart { service, .. } => assert_eq!(service, "svc"),
            other => {
                eyre::bail!("expected restart command, got {other:?}");
            }
        }
        assert!(matches!(
            commands_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        Ok(())
    }

    #[tokio::test]
    async fn stop_key_only_sends_for_a_live_dynamic_service() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["sh", "-c", "true"]
        "#};
        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!(err.to_string()))?;
        let mux = std::sync::Arc::new(
            micromux::Micromux::new(&parsed).map_err(|err| eyre::eyre!(err.to_string()))?,
        );
        let shutdown = micromux::CancellationToken::new();
        let (_runner, handles) = mux.start(shutdown.clone());
        let (commands_tx, mut commands_rx) = mpsc::channel(4);
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(
                handles.reader.clone(),
                commands_tx.clone(),
            )),
            Some(commands_tx),
            shutdown,
            true,
        );
        let stop_key = KeyEvent {
            code: KeyCode::Char('s'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        app.handle_key_press(stop_key);
        assert!(matches!(
            commands_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        if let Some(service) = app.state.current_service_mut() {
            service.snapshot.origin = micromux::OriginKind::Dynamic;
            service.snapshot.dynamic = Some(micromux::DynamicServiceInfo {
                created_at_unix_ms: 1,
                expires_at_unix_ms: None,
                owner: None,
                revision: 1,
            });
        }
        app.handle_key_press(stop_key);
        match commands_rx.try_recv()? {
            micromux::Command::StopDynamic { service, ack } => {
                assert_eq!(service, "svc");
                assert!(ack.is_none());
            }
            other => {
                eyre::bail!("expected stop-dynamic command, got {other:?}");
            }
        }

        if let Some(service) = app.state.current_service_mut() {
            service.snapshot.retired = Some(micromux::RetiredReason::Stopped);
        }
        app.handle_key_press(stop_key);
        assert!(matches!(
            commands_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        Ok(())
    }

    #[tokio::test]
    async fn resync_reconciles_rows_and_preserves_selection_by_id() -> eyre::Result<()> {
        fn handles_for(yaml: &str) -> eyre::Result<micromux::Handles> {
            let mut diagnostics: Vec<Diagnostic<usize>> = Vec::new();
            let parsed = micromux::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)
                .map_err(|err| eyre::eyre!(err.to_string()))?;
            let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
            let (_runner, handles) = mux.start(micromux::CancellationToken::new());
            Ok(handles)
        }

        let initial = handles_for(indoc! {r#"
            version: 1
            services:
              a: { command: ["true"] }
              b: { command: ["true"] }
        "#})?;
        let shutdown = micromux::CancellationToken::new();
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(initial.reader, initial.commands.clone())),
            Some(initial.commands),
            shutdown,
            true,
        );
        app.state.selected_service = 1;
        if let Some(service) = app.state.current_service_mut() {
            service.cached_text = ratatui::text::Text::from("preserved");
        }

        let updated = handles_for(indoc! {r#"
            version: 1
            services:
              b: { command: ["true"] }
              c: { command: ["true"] }
        "#})?;
        app.source = SessionSource::Local(LocalSource::new(updated.reader, updated.commands));
        app.resync();

        assert_eq!(
            app.state
                .services
                .iter()
                .map(|service| service.snapshot.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c"]
        );
        assert_eq!(app.state.selected_service, 0);
        assert_eq!(
            app.state
                .current_service()
                .map(|service| service.cached_text.clone()),
            Some(ratatui::text::Text::from("preserved"))
        );
        Ok(())
    }
}
