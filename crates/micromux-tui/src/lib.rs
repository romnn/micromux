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
    pub attach_mode: bool,
    pub focus: Focus,
    pub terminal_rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Services,
    Logs,
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
                };
                (service_id.clone(), service_state)
            })
            .collect();

        let log_view = render::log_view::LogView::default();

        Self {
            running: true,
            commands_tx,
            shutdown,
            ui_rx,
            input_event_handler: event::InputHandler::new(),
            state: state::State::new(services),
            log_view,
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
                Some(Event::Input(event::Input::Tick)) => {
                    terminal.draw(|frame| frame.render_widget(&mut self, frame.area()))?;
                }
                Some(Event::Input(event)) => {
                    tracing::debug!(%event, "received event");
                }
                Some(Event::Scheduler(event)) => {
                    tracing::debug!(%event, "received event");
                }
                _ => {}
            }

            match event {
                Some(Event::Input(event)) => {
                    self.handle_input_event(event)?;
                }
                Some(Event::Scheduler(event)) => {
                    self.handle_event(event)?;
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
                line,
            } => {
                let service = self.state.services.get_mut(&service_id).unwrap();
                let line = match stream {
                    micromux::scheduler::OutputStream::Stdout => line,
                    micromux::scheduler::OutputStream::Stderr => format!("[stderr] {line}"),
                };
                service.logs.push(line);
            }
            SchedulerEvent::Killed(service_id) => {
                let service = self.state.services.get_mut(&service_id).unwrap();
                service.exec_state = state::Execution::Killed;
            }
            SchedulerEvent::Exited(service_id, _) => {
                let service = self.state.services.get_mut(&service_id).unwrap();
                service.exec_state = state::Execution::Exited;
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
                                self.attach_mode = false;
                            }
                            _ => {
                                if let Some(bytes) = key_event_to_bytes(key.code, key.modifiers) {
                                    let service_id = self.state.current_service().id.clone();
                                    let _ = self
                                        .commands_tx
                                        .try_send(micromux::scheduler::Command::SendInput(
                                            service_id,
                                            bytes,
                                        ));
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
                            self.focus = match self.focus {
                                Focus::Services => Focus::Logs,
                                Focus::Logs => Focus::Services,
                            };
                        }
                        // Toggle attach mode
                        KeyCode::Char('a') => {
                            self.attach_mode = !self.attach_mode;
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
                        },
                        KeyCode::Char('j') | KeyCode::Down => match self.focus {
                            Focus::Services => self.state.service_down(),
                            Focus::Logs => self.scroll_logs_down(1),
                        },
                        KeyCode::Char('g') => {
                            if self.focus == Focus::Logs {
                                self.log_view.follow_tail = false;
                                self.log_view.scroll_offset = 0;
                            }
                        }
                        KeyCode::Char('G') => {
                            if self.focus == Focus::Logs {
                                self.log_view.follow_tail = true;
                            }
                        }
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
                            self.log_view.wrap = !self.log_view.wrap;
                        }
                        // Toggle automatic tailing for log viewer
                        KeyCode::Char('t') => {
                            self.log_view.follow_tail = !self.log_view.follow_tail;
                        }
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
        self.log_view.scroll_offset = self.log_view.scroll_offset.saturating_add(lines).min(max_off);
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
        let _ = self
            .commands_tx
            .try_send(micromux::scheduler::Command::Disable(service.id.clone()));
    }

    /// Restart service
    fn restart_current_service(&self) {
        let service = self.state.current_service();
        tracing::info!(service_id = service.id, "restarting service");
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

    if modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        match code {
            KeyCode::Char('c') => return Some(vec![0x03]),
            KeyCode::Char('d') => return Some(vec![0x04]),
            KeyCode::Char('z') => return Some(vec![0x1a]),
            _ => {}
        }
    }

    match code {
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Char(c) => Some(c.to_string().into_bytes()),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        _ => None,
    }
}
