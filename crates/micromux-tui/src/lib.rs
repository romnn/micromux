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
    pub shutdown: micromux::CancellationToken,
    pub ui_rx: UiEventStream,
    /// Event handler
    pub input_event_handler: event::InputHandler,
    /// Current state
    pub state: state::State,
    /// Log viewer
    pub log_view: crate::render::log_view::LogView,
}

impl App {
    pub fn new(
        services: &ServiceMap,
        ui_rx: mpsc::Receiver<SchedulerEvent>,
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
                    open_ports: vec![],
                    logs: BoundedLog::with_limits(1000, 64 * MiB).into(),
                };
                (service_id.clone(), service_state)
            })
            .collect();

        let log_view = render::log_view::LogView::default();

        Self {
            running: true,
            shutdown,
            ui_rx,
            input_event_handler: event::InputHandler::new(),
            state: state::State::new(services),
            log_view,
        }
    }
}

impl App {
    pub async fn run(mut self, mut terminal: DefaultTerminal) -> eyre::Result<()> {
        #[derive(Debug)]
        enum Event {
            Input(event::Input),
            Scheduler(SchedulerEvent),
        }

        let debounce_duration = Duration::from_millis(100);
        let mut pending = false;

        while self.is_running() {
            tracing::debug!("render frame");

            terminal.draw(|frame| frame.render_widget(&mut self, frame.area()))?;

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

            let mut new_logs_subscription = self.state.current_service().logs.subscribe();

            // Wait until an (input) event is received.
            let event = tokio::select! {
                _ = self.shutdown.cancelled() => None,
                // _ = new_logs_subscription.changed() => None,
                event = self.ui_rx.next() => event.map(Event::Scheduler),
                input = self.input_event_handler.next() => Some(Event::Input(input?)),
            };

            tracing::debug!(?event, "received event");

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
            SchedulerEvent::Started {
                service_id,
                stderr,
                stdout,
            } => {
                use futures::{AsyncBufReadExt, StreamExt};

                let service = self.state.services.get(&service_id).unwrap();

                if let Some(stderr) = stderr {
                    tokio::spawn({
                        let logs = service.logs.clone();
                        let service_id = service_id.clone();
                        async move {
                            let mut lines = futures::io::BufReader::new(stderr).lines();
                            while let Some(line) = lines.next().await {
                                tracing::trace!(?line, service_id, "read stderr line");
                                match line {
                                    Ok(line) => logs.push(line),
                                    Err(err) => {
                                        tracing::warn!(
                                            ?err,
                                            service_id,
                                            "failed to read stderr line"
                                        );
                                    }
                                }
                            }
                        }
                    });
                }

                if let Some(stdout) = stdout {
                    tokio::spawn({
                        let logs = service.logs.clone();
                        let service_id = service_id.clone();
                        async move {
                            let mut lines = futures::io::BufReader::new(stdout).lines();
                            while let Some(line) = lines.next().await {
                                tracing::trace!(?line, service_id, "read stdout line");
                                match line {
                                    Ok(line) => logs.push(line),
                                    Err(err) => {
                                        tracing::warn!(
                                            ?err,
                                            service_id,
                                            "failed to read stdout line"
                                        )
                                    }
                                }
                            }
                        }
                    });
                }
            }
            SchedulerEvent::Killed(service_id) => {}
            SchedulerEvent::Exited(service_id, _) => {}
            SchedulerEvent::Healthy(service_id) => {}
            SchedulerEvent::Unhealthy(service_id) => {}
            SchedulerEvent::Disabled(service_id) => {}
        }
        Ok(())
    }

    fn handle_input_event(&mut self, input_event: event::Input) -> eyre::Result<()> {
        use crossterm::event::{
            DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind,
        };

        match input_event {
            event::Input::Tick => self.tick(),
            event::Input::Event(event) => match event {
                crossterm::event::Event::Key(key) if key.kind == KeyEventKind::Press => {
                    match key.code {
                        // Quit
                        KeyCode::Char('q') | KeyCode::Esc => self.exit(),
                        // Disable current service
                        KeyCode::Char('d') => self.disable_current_service(),
                        // Restart service
                        KeyCode::Char('r') => self.restart_current_service(),
                        // Restart all services
                        KeyCode::Char('R') => self.restart_all_services(),
                        // Select service above current service (move up)
                        KeyCode::Char('k') | KeyCode::Up => self.state.service_up(),
                        // Select service below current service (move down)
                        KeyCode::Char('j') | KeyCode::Down => self.state.service_down(),
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
                        KeyCode::Char('w') => {
                            self.log_view.follow_tail = !self.log_view.follow_tail;
                        }
                        // scroll up manually
                        //         KeyCode::Up => {
                        //             self.follow_tail = false;
                        //             self.scroll_offset = self.scroll_offset.saturating_sub(1);
                        //         }
                        //         // scroll down manually
                        //         KeyCode::Down => {
                        //             self.follow_tail = false;
                        //             let max_off = total_lines.saturating_sub(area_height as usize) as u16;
                        //             self.scroll_offset = (self.scroll_offset + 1).min(max_off);
                        //         }
                        _ => {}
                    }
                }
                _ => {}
            },
        }
        Ok(())
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
        // self.mux.disable_service(service.id);
    }

    /// Restart service
    fn restart_current_service(&self) {
        let service = self.state.current_service();
        tracing::info!(service_id = service.id, "restarting service");
        // self.mux.restart_service(service.id);
    }

    // /// Restart service
    // fn restart_service(&mut self, service: ) {
    //     let service = self.state.current_service();
    //     tracing::info!(service_id = service.id, "restarting service");
    // }

    /// Restart all services
    fn restart_all_services(&self) {
        tracing::info!("restarting all services");
        for service in self.state.services.iter() {}
    }
}
