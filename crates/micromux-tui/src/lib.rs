#![allow(warnings)]
#![deny(unused_must_use)]

pub mod event;
pub mod render;
pub mod state;
pub mod style;

pub use crossterm;
pub use ratatui;

use color_eyre::eyre;
use futures::StreamExt;
use micromux::{Micromux, ServiceMap, bounded_log::BoundedLog, scheduler::Event as SchedulerEvent};
use ratatui::{
    DefaultTerminal, Terminal,
    backend::{Backend, CrosstermBackend},
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    prelude::*,
    style::Styled,
    style::{Color, Modifier, Style, palette::tailwind},
    text::Span,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Widget},
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

pub type UiEventStream = futures::stream::Chain<
    ReceiverStream<SchedulerEvent>,
    futures::stream::Pending<SchedulerEvent>,
>;

pub const KiB: usize = 1024;
pub const MiB: usize = 1024 * KiB;
pub const GiB: usize = 1024 * MiB;
pub const HEALTHCHECK_HISTORY: usize = 2;

#[derive()]
pub struct App {
    /// Running state of the TUI application.
    pub running: bool,
    pub commands_tx: mpsc::Sender<micromux::scheduler::Command>,
    pub shutdown: micromux::CancellationToken,
    pub ui_rx: UiEventStream,
    /// Event handler
    pub input_event_handler: event::InputHandler,
    /// Current state
    pub state: state::State,
    /// Log viewer
    pub log_view: crate::render::log_view::LogView,
    pub healthcheck_view: crate::render::log_view::LogView,
    pub show_healthcheck_pane: bool,
    pub attach_mode: bool,
    pub focus: Focus,
    pub terminal_rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Services,
    Logs,
    Healthcheck,
}

impl App {
    pub fn new(
        services: &ServiceMap,
        ui_rx: mpsc::Receiver<SchedulerEvent>,
        commands_tx: mpsc::Sender<micromux::scheduler::Command>,
        shutdown: micromux::CancellationToken,
    ) -> Self {
        let mut ui_rx = ReceiverStream::new(ui_rx).chain(futures::stream::pending());

        let services = services
            .iter()
            .map(|(service_id, service)| {
                let service_state = state::Service {
                    name: service.name.as_ref().to_string(),
                    id: service.id.clone(),
                    exec_state: state::Execution::Pending,
                    open_ports: service.open_ports.clone(),
                    logs: BoundedLog::with_limits(1000, 64 * MiB).into(),
                    cached_num_lines: 0,
                    cached_logs: String::new(),
                    logs_dirty: true,
                    healthcheck_configured: service.health_check.is_some(),
                    healthcheck_attempts: std::collections::VecDeque::new(),
                    healthcheck_cached_num_lines: 0,
                    healthcheck_cached_text: String::new(),
                    healthcheck_dirty: true,
                };
                (service_id.clone(), service_state)
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
            terminal_rows: 24,
        }
    }
}

impl App {
    pub async fn run(mut self, mut terminal: DefaultTerminal) -> eyre::Result<()> {
        #[derive(Debug, strum::Display)]
        enum Event {
            Input(event::Input),
            Scheduler(SchedulerEvent),
        }

        let area = terminal.size()?;
        self.terminal_rows = area.height;
        let _ = self
            .commands_tx
            .send(micromux::scheduler::Command::ResizeAll {
                cols: area.width,
                rows: area.height,
            })
            .await;

        while self.is_running() {
            // tracing::trace!("render frame");

            // Debounce timer -> perform redraw if pending
            // tokio::select! {
            //     _ = async {
            //         if pending {
            //             tokio::time::sleep(debounce_duration).await;
            //         } else {
            //             futures::future::pending::<()>().await;
            //         }
            //     } => {
            //         if pending {
            //             terminal.draw(|frame| frame.render_widget(&self, frame.area()))?;
            //             pending = false;
            //         }
            //     }
            // }
            // let mut new_logs_subscription = self.state.current_service().logs.subscribe();

            // Wait until an (input) event is received.
            let event = tokio::select! {
                _ = self.shutdown.cancelled() => None,
                input = self.input_event_handler.next() => Some(Event::Input(input?)),
                event = self.ui_rx.next() => event.map(Event::Scheduler),
                // _ = new_logs_subscription.changed() => None,
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
                    self.handle_input_event(event)?;
                    terminal.draw(|frame| frame.render_widget(&mut self, frame.area()))?;
                }
                Some(Event::Scheduler(event)) => {
                    self.handle_event(event)?;
                    terminal.draw(|frame| frame.render_widget(&mut self, frame.area()))?;
                }
                None => {}
            };
        }
        Ok(())
    }

    fn handle_event(&mut self, event: SchedulerEvent) -> eyre::Result<()> {
        match event {
            SchedulerEvent::Started { service_id } => {
                let service = self.state.services.get_mut(&service_id).unwrap();
                service.exec_state = state::Execution::Running { health: None };
            }
            SchedulerEvent::LogLine {
                service_id,
                stream,
                update,
                line,
            } => {
                let service = self.state.services.get_mut(&service_id).unwrap();
                let line = match stream {
                    micromux::scheduler::OutputStream::Stdout => line,
                    micromux::scheduler::OutputStream::Stderr => format!("[stderr] {line}"),
                };

                match update {
                    micromux::scheduler::LogUpdateKind::Append => {
                        service.logs.push(line);
                    }
                    micromux::scheduler::LogUpdateKind::ReplaceLast => {
                        service.logs.replace_last(line);
                    }
                }
                service.logs_dirty = true;
            }
            SchedulerEvent::Killed(service_id) => {
                let service = self.state.services.get_mut(&service_id).unwrap();
                if service.exec_state != state::Execution::Disabled {
                    service.exec_state = state::Execution::Killed;
                }
            }
            SchedulerEvent::Exited(service_id, _) => {
                let service = self.state.services.get_mut(&service_id).unwrap();
                if service.exec_state != state::Execution::Disabled {
                    service.exec_state = state::Execution::Exited;
                }
            }
            SchedulerEvent::Healthy(service_id) => {
                let service = self.state.services.get_mut(&service_id).unwrap();
                service.exec_state = state::Execution::Running {
                    health: Some(state::Health::Healthy),
                };
            }
            SchedulerEvent::Unhealthy(service_id) => {
                let service = self.state.services.get_mut(&service_id).unwrap();
                service.exec_state = state::Execution::Running {
                    health: Some(state::Health::Unhealthy),
                };
            }
            SchedulerEvent::Disabled(service_id) => {
                let service = self.state.services.get_mut(&service_id).unwrap();
                service.exec_state = state::Execution::Disabled;
            }
            SchedulerEvent::HealthCheckStarted {
                service_id,
                attempt,
                command,
            } => {
                let service = self.state.services.get_mut(&service_id).unwrap();
                while service.healthcheck_attempts.len() >= HEALTHCHECK_HISTORY {
                    service.healthcheck_attempts.pop_front();
                }

                service
                    .healthcheck_attempts
                    .push_back(state::HealthCheckAttempt {
                        id: attempt,
                        command,
                        output: BoundedLog::with_limits(200, 256 * KiB),
                        result: None,
                    });
                service.healthcheck_dirty = true;
            }
            SchedulerEvent::HealthCheckLogLine {
                service_id,
                attempt,
                stream,
                line,
            } => {
                let service = self.state.services.get_mut(&service_id).unwrap();
                if let Some(attempt_entry) = service
                    .healthcheck_attempts
                    .iter_mut()
                    .find(|a| a.id == attempt)
                {
                    let line = match stream {
                        micromux::scheduler::OutputStream::Stdout => line,
                        micromux::scheduler::OutputStream::Stderr => format!("[stderr] {line}"),
                    };
                    attempt_entry.output.push(line);
                    service.healthcheck_dirty = true;
                }
            }
            SchedulerEvent::HealthCheckFinished {
                service_id,
                attempt,
                success,
                exit_code,
            } => {
                let service = self.state.services.get_mut(&service_id).unwrap();
                if let Some(attempt_entry) = service
                    .healthcheck_attempts
                    .iter_mut()
                    .find(|a| a.id == attempt)
                {
                    attempt_entry.result = Some(state::HealthCheckResult { success, exit_code });
                    service.healthcheck_dirty = true;
                }
            }
        }
        Ok(())
    }

    fn handle_input_event(&mut self, input_event: event::Input) -> eyre::Result<()> {
        use crossterm::event::{
            DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        };

        match input_event {
            event::Input::Tick => self.tick(),
            event::Input::Event(event) => match event {
                crossterm::event::Event::Resize(cols, rows) => {
                    self.terminal_rows = rows;
                    let _ = self
                        .commands_tx
                        .try_send(micromux::scheduler::Command::ResizeAll { cols, rows });
                }
                crossterm::event::Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if self.attach_mode {
                        match key.code {
                            KeyCode::Esc => {
                                if key.modifiers.contains(KeyModifiers::ALT) {
                                    self.attach_mode = false;
                                } else if let Some(bytes) =
                                    key_event_to_bytes(key.code, key.modifiers)
                                {
                                    let service_id = self.state.current_service().id.clone();
                                    let _ = self.commands_tx.try_send(
                                        micromux::scheduler::Command::SendInput(service_id, bytes),
                                    );
                                }
                            }
                            _ => {
                                if let Some(bytes) = key_event_to_bytes(key.code, key.modifiers) {
                                    let service_id = self.state.current_service().id.clone();
                                    let _ = self.commands_tx.try_send(
                                        micromux::scheduler::Command::SendInput(service_id, bytes),
                                    );
                                }
                            }
                        }
                        return Ok(());
                    }

                    match key.code {
                        // Quit
                        KeyCode::Char('q') | KeyCode::Esc => self.exit(),
                        // Toggle focus
                        KeyCode::Tab => {
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
                        // Toggle attach mode
                        KeyCode::Char('a') => {
                            self.attach_mode = !self.attach_mode;
                        }
                        KeyCode::Char('H') => {
                            self.show_healthcheck_pane = !self.show_healthcheck_pane;
                            if !self.show_healthcheck_pane && self.focus == Focus::Healthcheck {
                                self.focus = Focus::Logs;
                            }
                        }
                        // Disable current service
                        KeyCode::Char('d') => self.disable_current_service(),
                        // Restart service
                        KeyCode::Char('r') => self.restart_current_service(),
                        // Restart all services
                        KeyCode::Char('R') => self.restart_all_services(),
                        // Navigation
                        KeyCode::Char('k') | KeyCode::Up => match self.focus {
                            Focus::Services => self.state.service_up(),
                            Focus::Logs => self.scroll_logs_up(1),
                            Focus::Healthcheck => self.scroll_healthchecks_up(1),
                        },
                        KeyCode::Char('j') | KeyCode::Down => match self.focus {
                            Focus::Services => self.state.service_down(),
                            Focus::Logs => self.scroll_logs_down(1),
                            Focus::Healthcheck => self.scroll_healthchecks_down(1),
                        },
                        KeyCode::Char('g') => match self.focus {
                            Focus::Logs => {
                                self.log_view.follow_tail = false;
                                self.log_view.scroll_offset = 0;
                            }
                            Focus::Healthcheck => {
                                self.healthcheck_view.follow_tail = false;
                                self.healthcheck_view.scroll_offset = 0;
                            }
                            Focus::Services => {}
                        },
                        KeyCode::Char('G') => match self.focus {
                            Focus::Logs => {
                                self.log_view.follow_tail = true;
                            }
                            Focus::Healthcheck => {
                                self.healthcheck_view.follow_tail = true;
                            }
                            Focus::Services => {}
                        },
                        // Decrease service sidebar width (resize to the left)
                        KeyCode::Char('-') | KeyCode::Char('h') | KeyCode::Left => {
                            self.state.resize_left()
                        }
                        // Increase service sidebar width (resize to the right)
                        KeyCode::Char('+') | KeyCode::Char('l') | KeyCode::Right => {
                            self.state.resize_right()
                        }
                        // Toggle wrapping for log viewer
                        KeyCode::Char('w') => {
                            let wrap = !self.log_view.wrap;
                            self.log_view.wrap = wrap;
                            self.healthcheck_view.wrap = wrap;
                        }
                        // Toggle automatic tailing for log viewer
                        KeyCode::Char('t') => match self.focus {
                            Focus::Logs | Focus::Services => {
                                self.log_view.follow_tail = !self.log_view.follow_tail;
                            }
                            Focus::Healthcheck => {
                                self.healthcheck_view.follow_tail =
                                    !self.healthcheck_view.follow_tail;
                            }
                        },
                        _ => {}
                    }
                }
                _ => {}
            },
        }
        Ok(())
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
        let (num_lines, _) = self.state.current_service().logs.full_text();
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
        let num_lines = self.state.current_service().healthcheck_cached_num_lines;
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
        let service = self.state.current_service();
        tracing::info!(service_id = service.id, "disabling service");
        let cmd = match service.exec_state {
            state::Execution::Disabled => micromux::scheduler::Command::Enable(service.id.clone()),
            _ => micromux::scheduler::Command::Disable(service.id.clone()),
        };
        let _ = self.commands_tx.try_send(cmd);
    }

    /// Restart service
    fn restart_current_service(&self) {
        let service = self.state.current_service();
        tracing::info!(service_id = service.id, "restarting service");
        if service.exec_state == state::Execution::Disabled {
            let _ = self
                .commands_tx
                .try_send(micromux::scheduler::Command::Enable(service.id.clone()));
        }
        let _ = self
            .commands_tx
            .try_send(micromux::scheduler::Command::Restart(service.id.clone()));
    }

    // /// Restart service
    // fn restart_service(&mut self, service: ) {
    //     let service = self.state.current_service();
    //     tracing::info!(service_id = service.id, "restarting service");
    // }

    /// Restart all services
    fn restart_all_services(&self) {
        tracing::info!("restarting all services");
        let _ = self
            .commands_tx
            .try_send(micromux::scheduler::Command::RestartAll);
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
                if ('a'..='z').contains(&c) {
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
    use super::{App, Focus, key_event_to_bytes};
    use codespan_reporting::diagnostic::Diagnostic;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use micromux::{ServiceMap, config};
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
    async fn tab_cycles_focus_with_and_without_healthcheck_pane() {
        let yaml = r#"
version: 1
services:
  svc:
    command: ["sh", "-c", "true"]
"#;
        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed =
            config::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics).unwrap();
        let config_dir = parsed.config_dir.clone();
        let services: ServiceMap = parsed
            .config
            .services
            .iter()
            .map(|(name, service_config)| {
                let service_id = name.as_ref().to_string();
                let service = micromux::service::Service::new(
                    name.as_ref().clone(),
                    &config_dir,
                    service_config.clone(),
                )
                .unwrap();
                (service_id, service)
            })
            .collect();

        let (_ui_tx, ui_rx) = mpsc::channel(1);
        let (cmd_tx, _cmd_rx) = mpsc::channel(1);
        let shutdown = micromux::CancellationToken::new();

        let mut app = App::new(&services, ui_rx, cmd_tx, shutdown);
        app.focus = Focus::Services;
        app.show_healthcheck_pane = false;

        app.handle_input_event(tab_press()).unwrap();
        assert_eq!(app.focus, Focus::Logs);
        app.handle_input_event(tab_press()).unwrap();
        assert_eq!(app.focus, Focus::Services);

        app.show_healthcheck_pane = true;
        app.focus = Focus::Services;
        app.handle_input_event(tab_press()).unwrap();
        assert_eq!(app.focus, Focus::Logs);
        app.handle_input_event(tab_press()).unwrap();
        assert_eq!(app.focus, Focus::Healthcheck);
        app.handle_input_event(tab_press()).unwrap();
        assert_eq!(app.focus, Focus::Services);
    }
}
