//! `micromux-tui` provides the terminal user interface for micromux.
//!
//! The main entry point is [`App`]. Most consumers construct an [`App`] from a list of
//! [`micromux::ServiceDescriptor`] values, then call [`App::render`].

mod event;
mod reducer;
mod render;
mod state;
mod style;

/// Re-export of `crossterm` for consumers that need to share types with the TUI.
pub use crossterm;
/// Re-export of `ratatui` for consumers that need to share types with the TUI.
pub use ratatui;

use color_eyre::eyre;
use futures::StreamExt;
use micromux::{BoundedLog, Command, Event as SchedulerEvent, ServiceDescriptor};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

type UiEventStream = futures::stream::Chain<
    ReceiverStream<SchedulerEvent>,
    futures::stream::Pending<SchedulerEvent>,
>;

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const HEALTHCHECK_HISTORY: usize = 2;

#[derive()]
/// Terminal application state.
pub struct App {
    /// Running state of the TUI application.
    running: bool,
    commands_tx: mpsc::Sender<Command>,
    shutdown: micromux::CancellationToken,
    ui_rx: UiEventStream,
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
    /// Construct a new [`App`].
    #[must_use]
    pub fn new(
        services: &[ServiceDescriptor],
        ui_rx: mpsc::Receiver<SchedulerEvent>,
        commands_tx: mpsc::Sender<Command>,
        shutdown: micromux::CancellationToken,
    ) -> Self {
        let ui_rx = ReceiverStream::new(ui_rx).chain(futures::stream::pending());

        let services = services
            .iter()
            .map(|service| {
                let service_state = state::Service {
                    id: service.id.clone(),
                    exec_state: state::Execution::Pending,
                    open_ports: service.open_ports.clone(),
                    logs: BoundedLog::with_limits(1000, 64 * MIB).into(),
                    cached_num_lines: 0,
                    cached_logs: String::new(),
                    logs_dirty: true,
                    healthcheck_configured: service.healthcheck_configured,
                    healthcheck_attempts: std::collections::VecDeque::new(),
                    healthcheck_cached_num_lines: 0,
                    healthcheck_cached_text: String::new(),
                    healthcheck_dirty: true,
                };
                (service.id.clone(), service_state)
            })
            .collect();

        let log_view = render::log_view::LogView::default();
        let healthcheck_view = render::log_view::LogView::default();

        Self {
            running: true,
            commands_tx,
            shutdown,
            ui_rx,
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
                Constraint::Length(0),
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

        self.last_pty_cols = cols;
        self.last_pty_rows = rows;
        let _ = self.commands_tx.try_send(Command::ResizeAll { cols, rows });
    }

    /// Run the TUI event loop.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Receiving an input event fails.
    /// - The underlying terminal backend fails to draw.
    pub async fn run(mut self, mut terminal: DefaultTerminal) -> eyre::Result<()> {
        #[derive(Debug, strum::Display)]
        enum Event {
            Input(event::Input),
            Scheduler(SchedulerEvent),
        }

        let area = terminal.size()?;
        self.terminal_cols = area.width;
        self.terminal_rows = area.height;
        self.maybe_resize_pty();

        while self.is_running() {
            // Wait until an (input) event is received.
            let event = tokio::select! {
                () = self.shutdown.cancelled() => None,
                input = self.input_event_handler.next() => Some(Event::Input(input?)),
                event = self.ui_rx.next() => event.map(Event::Scheduler),
            };

            match &event {
                Some(Event::Input(event)) if !event.is_tick() => {
                    tracing::trace!(%event, "received event");
                }
                Some(Event::Scheduler(event)) => {
                    tracing::debug!(%event, "received event");
                }
                _ => {}
            }

            match event {
                Some(Event::Input(event)) => {
                    self.handle_input_event(event);
                    terminal.draw(|frame| frame.render_widget(&mut self, frame.area()))?;
                }
                Some(Event::Scheduler(event)) => {
                    self.handle_event(event);
                    terminal.draw(|frame| frame.render_widget(&mut self, frame.area()))?;
                }
                None => {
                    self.running = false;
                }
            }
        }
        Ok(())
    }

    fn handle_event(&mut self, event: SchedulerEvent) {
        reducer::apply(&mut self.state, event);
    }

    fn handle_input_event(&mut self, input_event: event::Input) {
        match input_event {
            event::Input::Tick => self.tick(),
            event::Input::Event(event) => self.handle_crossterm_event(&event),
        }
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
                    let service_id = service.id.clone();
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
        // total rows minus footer (1) minus logs block borders (2)
        self.terminal_rows.saturating_sub(3)
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
        let (num_lines, _) = service.logs.full_text();
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

    /// Handles the tick event of the terminal.
    pub fn tick(&self) {}

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
        tracing::info!(service_id = service.id, "disabling service");
        let command = match service.exec_state {
            state::Execution::Disabled => Command::Enable(service.id.clone()),
            _ => Command::Disable(service.id.clone()),
        };
        let _ = self.commands_tx.try_send(command);
    }

    /// Restart service
    fn restart_current_service(&self) {
        let Some(service) = self.state.current_service() else {
            return;
        };
        tracing::info!(service_id = service.id, "restarting service");
        if service.exec_state == state::Execution::Disabled {
            let _ = self
                .commands_tx
                .try_send(Command::Enable(service.id.clone()));
        }
        let _ = self
            .commands_tx
            .try_send(Command::Restart(service.id.clone()));
    }

    /// Restart all services
    fn restart_all_services(&self) {
        tracing::info!("restarting all services");
        let _ = self.commands_tx.try_send(Command::RestartAll);
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
        let mux = micromux::Micromux::new(&parsed)
            .map_err(|err| color_eyre::eyre::eyre!(err.to_string()))?;
        let services = mux.services();

        let (_ui_tx, ui_rx) = mpsc::channel(1);
        let (cmd_tx, _cmd_rx) = mpsc::channel(1);
        let shutdown = micromux::CancellationToken::new();

        let mut app = App::new(&services, ui_rx, cmd_tx, shutdown);
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
}
