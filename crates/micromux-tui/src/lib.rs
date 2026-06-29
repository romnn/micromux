//! `micromux-tui` provides the terminal user interface for micromux.
//!
//! The main entry point is [`App`], constructed from a [`micromux::SessionModelReader`] (the source
//! of all domain state), a command sender, and a shutdown token; call [`App::render`] to run it. The
//! TUI holds only view state and reads the model on each [`micromux::SessionChange`].

mod event;
mod render;
mod state;
mod style;

use color_eyre::eyre;
use micromux::{ChangeKind, Command, SessionChange, SessionModelReader};
use ratatui::DefaultTerminal;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

/// Terminal application state.
pub struct App {
    /// Running state of the TUI application.
    running: bool,
    commands_tx: mpsc::Sender<Command>,
    shutdown: micromux::CancellationToken,
    /// Read capability over the authoritative session model — the single source of domain state.
    reader: SessionModelReader,
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
    attach_mode: bool,
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
    /// Construct a new [`App`] over the authoritative session model.
    #[must_use]
    pub fn new(
        reader: SessionModelReader,
        commands_tx: mpsc::Sender<Command>,
        shutdown: micromux::CancellationToken,
    ) -> Self {
        let changes = reader.subscribe();
        let snapshots = reader.services();

        let services = snapshots
            .into_iter()
            .map(|snapshot| state::Service {
                snapshot,
                cached_num_lines: 0,
                cached_logs: String::new(),
                logs_dirty: true,
                healthcheck_cached_num_lines: 0,
                healthcheck_cached_text: String::new(),
                healthcheck_dirty: true,
            })
            .collect();

        let log_view = render::log_view::LogView::default();
        let healthcheck_view = render::log_view::LogView::default();

        Self {
            running: true,
            commands_tx,
            shutdown,
            reader,
            changes,
            input_event_handler: event::InputHandler::new(),
            state: state::State::new(services),
            log_view,
            healthcheck_view,
            show_healthcheck_pane: false,
            attach_mode: false,
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
        let (cols, rows) = self.desired_pty_size();
        if cols == self.last_pty_cols && rows == self.last_pty_rows {
            return;
        }

        // Only advance the dedup cache once the resize is actually queued; otherwise a dropped
        // ResizeAll (full channel) would leave the cache claiming the new size and never retry.
        if self
            .commands_tx
            .try_send(Command::ResizeAll { cols, rows })
            .is_ok()
        {
            self.last_pty_cols = cols;
            self.last_pty_rows = rows;
        }
    }

    /// Run the TUI event loop.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Receiving an input event fails.
    /// - The underlying terminal backend fails to draw.
    pub async fn run(mut self, mut terminal: DefaultTerminal) -> eyre::Result<()> {
        enum Wake {
            Input(event::Input),
            Change(SessionChange),
            /// The change broadcast lagged (or this is the initial draw): re-read everything.
            Resync,
        }

        let area = terminal.size()?;
        self.terminal_cols = area.width;
        self.terminal_rows = area.height;
        self.maybe_resize_pty();

        // Seed the view from the model before the first frame.
        self.resync();
        terminal.draw(|frame| frame.render_widget(&mut self, frame.area()))?;

        while self.is_running() {
            let wake = tokio::select! {
                () = self.shutdown.cancelled() => None,
                input = self.input_event_handler.next() => Some(Wake::Input(input?)),
                change = self.changes.recv() => match change {
                    Ok(change) => Some(Wake::Change(change)),
                    Err(broadcast::error::RecvError::Lagged(_)) => Some(Wake::Resync),
                    Err(broadcast::error::RecvError::Closed) => None,
                },
            };

            match wake {
                Some(Wake::Input(event)) => self.handle_input_event(event),
                Some(Wake::Change(change)) => self.apply_change(&change),
                Some(Wake::Resync) => self.resync(),
                None => {
                    self.running = false;
                    continue;
                }
            }
            terminal.draw(|frame| frame.render_widget(&mut self, frame.area()))?;
        }
        Ok(())
    }

    /// Apply a single liveness notification by re-querying the model for the affected service. The
    /// broadcast carries only `{service_id, kind}`; the content lives in the model.
    fn apply_change(&mut self, change: &SessionChange) {
        match change.kind {
            ChangeKind::Status => {
                if let Some(snapshot) = self.reader.service(&change.service_id)
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
        }
    }

    /// Re-read every service snapshot and mark caches dirty (used on first draw and after a lag).
    fn resync(&mut self) {
        for snapshot in self.reader.services() {
            if let Some(service) = self.service_mut(&snapshot.id) {
                service.snapshot = snapshot;
                service.logs_dirty = true;
                service.healthcheck_dirty = true;
            }
        }
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
            _ => {}
        }
    }

    fn handle_key_press(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;

        if self.attach_mode {
            self.handle_key_press_attach_mode(key);
            return;
        }

        match key.code {
            // Quit
            KeyCode::Char('q') => self.exit(),

            // Toggle focus
            KeyCode::Tab => self.toggle_focus(),

            // Toggle attach mode
            KeyCode::Char('a') => {
                self.attach_mode = !self.attach_mode;
            }
            KeyCode::Char('H') => self.toggle_healthcheck_pane(),

            // Disable current service
            KeyCode::Char('d') => self.disable_current_service(),

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

    fn handle_key_press_attach_mode(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        match key.code {
            KeyCode::Esc if key.modifiers.contains(KeyModifiers::ALT) => {
                self.attach_mode = false;
            }
            _ => {
                if let Some(bytes) = key_event_to_bytes(key.code, key.modifiers)
                    && let Some(service) = self.state.current_service()
                {
                    let service_id = service.snapshot.id.clone();
                    let _ = self
                        .commands_tx
                        .try_send(Command::SendInput(service_id, bytes));
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
        let num_lines = service.cached_num_lines;
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
        // Send shutdown (cancellation) signal
        self.shutdown.cancel();
        self.running = false;
    }

    /// Disable service
    fn disable_current_service(&self) {
        let Some(service) = self.state.current_service() else {
            return;
        };
        tracing::info!(service_id = service.snapshot.id, "disabling service");
        let command = match service.snapshot.desired {
            micromux::Desired::Disabled => Command::enable(service.snapshot.id.clone()),
            micromux::Desired::Enabled => Command::disable(service.snapshot.id.clone()),
        };
        let _ = self.commands_tx.try_send(command);
    }

    /// Restart service
    fn restart_current_service(&self) {
        let Some(service) = self.state.current_service() else {
            return;
        };
        tracing::info!(service_id = service.snapshot.id, "restarting service");
        let _ = self
            .commands_tx
            .try_send(Command::restart(service.snapshot.id.clone()));
    }

    /// Restart all services
    fn restart_all_services(&self) {
        tracing::info!("restarting all services");
        let _ = self.commands_tx.try_send(Command::restart_all());
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
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use indoc::indoc;
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
    async fn tab_cycles_focus_with_and_without_healthcheck_pane() -> color_eyre::eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["sh", "-c", "true"]
        "#};
        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)
            .map_err(|err| color_eyre::eyre::eyre!(err.to_string()))?;
        let mux = std::sync::Arc::new(
            micromux::Micromux::new(&parsed)
                .map_err(|err| color_eyre::eyre::eyre!(err.to_string()))?,
        );
        let shutdown = micromux::CancellationToken::new();
        // The runner is not spawned; the model reader is seeded with initial snapshots and is all
        // the focus test needs.
        let (_runner, handles) = mux.start(shutdown.clone());

        let mut app = App::new(handles.reader.clone(), handles.commands.clone(), shutdown);
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
    async fn restart_key_on_disabled_service_does_not_send_enable() -> color_eyre::eyre::Result<()>
    {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["sh", "-c", "true"]
        "#};
        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)
            .map_err(|err| color_eyre::eyre::eyre!(err.to_string()))?;
        let mux = std::sync::Arc::new(
            micromux::Micromux::new(&parsed)
                .map_err(|err| color_eyre::eyre::eyre!(err.to_string()))?,
        );
        let shutdown = micromux::CancellationToken::new();
        let (_runner, handles) = mux.start(shutdown.clone());
        let (commands_tx, mut commands_rx) = mpsc::channel(4);

        let mut app = App::new(handles.reader.clone(), commands_tx, shutdown);
        if let Some(service) = app.state.current_service_mut() {
            service.snapshot.desired = micromux::Desired::Disabled;
        }

        app.restart_current_service();

        match commands_rx.try_recv()? {
            micromux::Command::Restart { service, .. } => assert_eq!(service, "svc"),
            other => color_eyre::eyre::bail!("expected restart command, got {other:?}"),
        }
        assert!(matches!(
            commands_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        Ok(())
    }
}
