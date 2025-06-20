#![allow(warnings)]

pub use crossterm;
pub use ratatui;

use color_eyre::eyre;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use itertools::intersperse;
use micromux::{Micromux, health_check::Health, scheduler::State, service::Service};
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
use std::{
    io,
    time::{Duration, Instant},
};

#[derive(Debug, Default, PartialEq, Eq)]
enum AppMode {
    #[default]
    Running,
    Quit,
}

// #[derive(Debug)]
// pub struct Service {
//     pub state: State,
//     pub health: Option<Health>,
//     pub name: String,
//     pub open_ports: Vec<u16>,
// }

pub fn health_style(service: &Service) -> Style {
    match service.health {
        Some(Health::Unhealthy) => Style::default().fg(tailwind::RED.c500),
        _ => Style::default().fg(Color::White).fg(tailwind::GREEN.c500),
    }
}

pub fn style(service: &Service) -> Style {
    match service.state {
        State::Disabled => Style::default().fg(Color::White).fg(tailwind::GRAY.c500),
        State::Pending => Style::default().fg(Color::White).fg(tailwind::BLUE.c500),
        State::Running { .. } | State::Killed { .. } | State::Exited { .. } => {
            health_style(service)
        }
    }
}

pub fn status(service: &Service) -> &'static str {
    match (&service.state, &service.health) {
        (state @ (State::Disabled | State::Pending | State::Exited { .. }), _) => state.into(),
        (_, Some(health)) => health.into(),
        (state, _) => state.into(),
    }
}

#[derive()]
pub struct App {
    pub mode: AppMode,
    pub mux: Arc<micromux::Micromux>,
    pub services_sidebar_width: u16,
    pub selected_service: usize,
    pub viewer_text: String,
    pub show_popup: bool,
}

const INITIAL_SIDEBAR_WIDTH: u16 = 40;
const MIN_SIDEBAR_WIDTH: u16 = 20;

impl App {
    pub fn new(mux: Arc<Micromux>) -> Self {
        Self {
            mode: AppMode::default(),
            mux,
            services_sidebar_width: INITIAL_SIDEBAR_WIDTH,
            selected_service: 0,
            show_popup: false,
            viewer_text: "This is the viewer output.\nYou can display multiline text here.".into(),
        }
    }
}

impl App {
    pub fn run(mut self, mut terminal: DefaultTerminal) -> eyre::Result<()> {
        while self.is_running() {
            terminal.draw(|frame| frame.render_widget(&self, frame.area()))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn handle_events(&mut self) -> eyre::Result<()> {
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => self.exit(),
                KeyCode::Char('d') => self.disable_service(),
                KeyCode::Char('r') => self.restart_service(),
                KeyCode::Char('R') => self.restart_all_services(),
                KeyCode::Char('k') | KeyCode::Up => self.service_up(),
                KeyCode::Char('j') | KeyCode::Down => self.service_down(),
                KeyCode::Char('-') | KeyCode::Char('h') | KeyCode::Left => self.resize_left(),
                KeyCode::Char('+') | KeyCode::Char('l') | KeyCode::Right => self.resize_right(),
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.mode == AppMode::Running
    }

    fn exit(&mut self) {
        // Send shutdown (cancellation) signal
        self.mux.cancel.cancel();
        self.mode = AppMode::Quit;
    }

    fn disable_service(&mut self) {
        // disable
    }

    fn restart_service(&mut self) {
        // restart
    }

    fn restart_all_services(&mut self) {
        // restart
    }

    /// Update the selection index.
    fn service_down(&mut self) {
        self.selected_service = self
            .selected_service
            .saturating_add(1)
            .min(self.mux.services.len().saturating_sub(1));
    }

    fn service_up(&mut self) {
        self.selected_service = self.selected_service.saturating_sub(1);
    }

    fn resize_left(&mut self) {
        self.services_sidebar_width = self
            .services_sidebar_width
            .saturating_sub(2)
            .max(MIN_SIDEBAR_WIDTH);
    }

    fn resize_right(&mut self) {
        self.services_sidebar_width = self.services_sidebar_width.saturating_add(2);
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_width = r.width * percent_x / 100;
    let popup_height = r.height * percent_y / 100;
    let popup_x = r.x + (r.width - popup_width) / 2;
    let popup_y = r.y + (r.height - popup_height) / 2;
    Rect {
        x: popup_x,
        y: popup_y,
        width: popup_width,
        height: popup_height,
    }
}

impl Widget for &App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let [header_area, main_area, footer_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(0), // header
                Constraint::Min(0),    // main area
                Constraint::Length(1), // footer
            ])
            .spacing(0)
            .areas(area);

        let [services_area, logs_area] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(self.services_sidebar_width),
                Constraint::Min(0),
            ])
            .spacing(0)
            .areas(main_area);

        App::header().render(header_area, buf);
        self.render_services(services_area, buf);
        self.render_logs(logs_area, buf);
        App::footer().render(footer_area, buf);
    }
}

impl App {
    const HEADER_COLOR: Color = tailwind::YELLOW.c500;
    const HIGHLIGHT_COLOR: Color = tailwind::GRAY.c900;
    // const AXIS_COLOR: Color = tailwind::BLUE.c300;

    fn render_services(&self, area: Rect, buf: &mut Buffer) {
        let items: Vec<ListItem> = self
            .mux
            .services
            .iter()
            // .map(|i| ListItem::new(i.name.as_str()))
            .map(|(name, service)| {
                // Paragraph::new(
                //     Line::from(
                //         footer_text
                //             .iter()
                //             .flat_map(|Keys { keys, description }| {
                //                 [
                //                     "   ".into(),
                //                     keys.fg(tailwind::YELLOW.c500).bold(),
                //                     format!(" {description}").fg(tailwind::GRAY.c500),
                //                 ]
                //             })
                //             .collect::<Vec<_>>(),
                //     )
                //     .left_aligned(),
                // )
                // .wrap(ratatui::widgets::Wrap { trim: false })

                // let name_span = Span::raw(&service.name);
                // let status_style = match service.state {
                //     State::Healthy | State::Running => Style::default().fg(tailwind::GREEN.c500),
                //     State::Starting => Style::default().fg(tailwind::YELLOW.c500),
                //     State::Unhealthy | State::Exited => Style::default().fg(tailwind::RED.c500),
                // };
                let status = format!("{: >10}", status(service)).set_style(style(service));
                // let fixed_latency = format!("{: <10}", service.latency);

                // let status_span = Span::styled(fixed_status, status_style);
                // let latency_span = Span::styled(fixed_latency, Style::default().fg(Color::Yellow));

                // Combine into one line.
                // let spans = Spans::from(vec![name_span, status_span, latency_span]);
                let ports = service
                    .open_ports
                    .iter()
                    .map(|i| format!(":{i}").fg(tailwind::GRAY.c400)); // .collect::<Vec<();

                let line = [status, " ".into(), service.id.as_str().into()]
                    .into_iter()
                    .chain(if ports.len() > 0 {
                        [" [".into()]
                            .into_iter()
                            .chain(intersperse(ports, ", ".into()))
                            .chain(["]".into()])
                            .collect()
                    } else {
                        vec!["".into()]
                    });

                // std::iter::empty()
                // format!(" [{}]", intersperse(ports, ", ".into()).collect::<Vec<_>>()).into()
                //     line =
                // }

                ListItem::new(Line::from_iter(line))
                // Span::from(vec![status, service.name.into()])
                // ListItem::new([
                //     Line::from("left").alignment(Alignment::Left),
                //     Line::from("center").alignment(Alignment::Center),
                //     Line::from("right").alignment(Alignment::Right),
                // ])
                // ListItem::new(i.name.as_str()))
            })
            .collect();

        let mut state = ListState::default();
        state.select(Some(self.selected_service));

        let sidebar = List::new(items)
            .block(Block::default().borders(Borders::ALL).title("Services"))
            .highlight_style(
                Style::default()
                    .bg(Self::HIGHLIGHT_COLOR)
                    // .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(" > ");
        StatefulWidget::render(&sidebar, area, buf, &mut state);
    }

    fn render_logs(&self, area: Rect, buf: &mut Buffer) {
        let viewer = Paragraph::new(self.viewer_text.as_str())
            .block(Block::default().borders(Borders::ALL).title("Logs"));
        Widget::render(&viewer, area, buf);
    }

    #[allow(unused)]
    fn header() -> impl Widget {
        let header = format!("micromux v{}", env!("CARGO_PKG_VERSION"))
            .bold()
            .fg(Self::HEADER_COLOR)
            .into_centered_line();
        Paragraph::new(header)
        // .block(Block::default().borders(Borders::BOTTOM))
        // Paragraph::new("micromux")
        //     .alignment(Alignment::Left)
        //     // .style(Style::default().fg(Self::HEADER_COLOR))
        //     .block(Block::default().borders(Borders::BOTTOM))
    }

    // Footer with commands.
    fn footer() -> impl Widget {
        let header = format!("micromux v{}", env!("CARGO_PKG_VERSION"))
            .bold()
            .fg(Self::HEADER_COLOR);

        #[derive(Debug)]
        struct Keys<'a> {
            keys: &'a str,
            description: &'a str,
        }

        impl<'a> Keys<'a> {
            fn new(keys: &'a str, description: &'a str) -> Self {
                Self { keys, description }
            }
        }

        let footer_text = [
            Keys::new("↑/↓", "Navigate"),
            Keys::new("←/→", "Resize"),
            Keys::new("r", "Restart"),
            Keys::new("R", "Restart All"),
            Keys::new("d", "Disable"),
            Keys::new("q", "Quit"),
        ];

        Paragraph::new(
            Line::from(
                footer_text
                    .iter()
                    .flat_map(|Keys { keys, description }| {
                        [
                            "   ".into(),
                            keys.fg(tailwind::YELLOW.c500).bold(),
                            format!(" {description}").fg(tailwind::GRAY.c500),
                        ]
                    })
                    .collect::<Vec<_>>(),
            )
            .left_aligned(),
        )
        .wrap(ratatui::widgets::Wrap { trim: false })
    }

    pub fn render(self) {
        let terminal = ratatui::init();
        let app_result = self.run(terminal);
        ratatui::restore();
    }
}

// pub fn render() -> eyre::Result<()> {
//     let terminal = ratatui::init();
//     let app_result = App::default().run(terminal);
//     ratatui::restore();
//     return app_result;
// }
