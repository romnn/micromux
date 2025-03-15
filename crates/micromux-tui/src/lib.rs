#![allow(warnings)]

use color_eyre::eyre;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use itertools::intersperse;
use ratatui::{DefaultTerminal, prelude::*, style::Styled};
use ratatui::{
    Terminal,
    backend::{Backend, CrosstermBackend},
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, palette::tailwind},
    text::Span,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Widget},
};
use std::{
    io,
    time::{Duration, Instant},
};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, strum::Display, strum::IntoStaticStr,
)]
pub enum Health {
    #[strum(serialize = "HEALTHY")]
    Healthy,
    #[strum(serialize = "UNHEALTHY")]
    Unhealthy,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, strum::Display, strum::IntoStaticStr,
)]
pub enum State {
    #[strum(serialize = "PENDING")]
    Pending,
    #[strum(serialize = "RUNNING")]
    Running,
    #[strum(serialize = "EXITED")]
    Exited,
    #[strum(serialize = "DISABLED")]
    Disabled,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum AppMode {
    #[default]
    Running,
    Quit,
}

#[derive(Debug)]
pub struct Service {
    pub state: State,
    pub health: Option<Health>,
    pub name: String,
    pub open_ports: Vec<u16>,
}

impl Service {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            state: State::Pending,
            health: None,
            open_ports: vec![],
        }
    }

    pub fn with_state(mut self, state: impl Into<State>) -> Self {
        self.state = state.into();
        self
    }

    pub fn with_health(mut self, health: impl Into<Health>) -> Self {
        self.health = Some(health.into());
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.open_ports.push(port);
        self
    }

    pub fn health_style(&self) -> Style {
        match self.health {
            Some(Health::Unhealthy) => Style::default().fg(tailwind::RED.c500),
            _ => Style::default().fg(Color::White).fg(tailwind::GREEN.c500),
        }
    }

    pub fn style(&self) -> Style {
        match self.state {
            State::Disabled => Style::default().fg(Color::White).fg(tailwind::GRAY.c500),
            State::Pending => Style::default().fg(Color::White).fg(tailwind::BLUE.c500),
            State::Running | State::Exited => self.health_style(),
        }
    }

    pub fn status(&self) -> &'static str {
        match (self.state, self.health) {
            (state @ (State::Disabled | State::Pending | State::Exited), _) => state.into(),
            (_, Some(health)) => health.into(),
            (state, _) => state.into(),
        }
    }
}

#[derive(Debug)]
pub struct App {
    mode: AppMode,
    services: Vec<Service>,
    services_sidebar_width: u16,
    selected_service: usize,
    viewer_text: String,
    show_popup: bool,
}

const INITIAL_SIDEBAR_WIDTH: u16 = 40;
const MIN_SIDEBAR_WIDTH: u16 = 20;

impl Default for App {
    fn default() -> Self {
        Self {
            mode: AppMode::default(),
            services: vec![
                Service::new("Service A").with_state(State::Running),
                Service::new("Service B")
                    .with_state(State::Pending)
                    .with_port(9000),
                Service::new("Service C").with_state(State::Disabled),
                Service::new("Service D")
                    .with_state(State::Running)
                    .with_health(Health::Unhealthy)
                    .with_port(8080),
            ],
            services_sidebar_width: INITIAL_SIDEBAR_WIDTH,
            selected_service: 0,
            show_popup: false,
            viewer_text: "This is the viewer output.\nYou can display multiline text here.".into(),
        }
    }
}

impl App {
    pub fn run(mut self, mut terminal: DefaultTerminal) -> eyre::Result<()> {
        // self.insert_test_defaults();
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
            .min(self.services.len().saturating_sub(1));
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
            .services
            .iter()
            // .map(|i| ListItem::new(i.name.as_str()))
            .map(|service| {
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
                let status = format!("{: >10}", service.status()).set_style(service.style());
                // let fixed_latency = format!("{: <10}", service.latency);

                // let status_span = Span::styled(fixed_status, status_style);
                // let latency_span = Span::styled(fixed_latency, Style::default().fg(Color::Yellow));

                // Combine into one line.
                // let spans = Spans::from(vec![name_span, status_span, latency_span]);
                let ports = service
                    .open_ports
                    .iter()
                    .map(|i| format!(":{i}").fg(tailwind::GRAY.c400)); // .collect::<Vec<();

                let line = [status, " ".into(), service.name.as_str().into()]
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
}

pub fn render() -> eyre::Result<()> {
    color_eyre::install()?;
    let terminal = ratatui::init();
    let app_result = App::default().run(terminal);
    ratatui::restore();
    return app_result;

    // // Terminal setup.
    // enable_raw_mode()?;
    // let mut stdout = io::stdout();
    // execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    // let backend = CrosstermBackend::new(stdout);
    // let mut terminal = Terminal::new(backend)?;
    //
    // let mut app = App::default();
    // let tick_rate = Duration::from_millis(200);
    // let mut last_tick = Instant::now();
    //
    // 'main_loop: loop {
    //     terminal.draw(|f| {
    //         let size = f.size();
    //
    //         // Split into header, main area, and footer.
    //         let chunks = Layout::default()
    //             .direction(Direction::Vertical)
    //             .constraints([
    //                 Constraint::Length(1), // header
    //                 Constraint::Min(0),    // main area
    //                 Constraint::Length(1), // footer
    //             ])
    //             .split(size);
    //
    //         // Header.
    //         let header = Paragraph::new("micromux")
    //             .alignment(Alignment::Right)
    //             .style(Style::default().fg(Color::Yellow))
    //             .block(Block::default().borders(Borders::BOTTOM));
    //         f.render_widget(header, chunks[0]);
    //
    //         // Footer with commands.
    //         let footer_text = [
    //             "←/→: Resize sidebar",
    //             "↑/↓: Navigate",
    //             "r: Restart",
    //             "R: Restart All",
    //             "d: Disable",
    //             "q: Toggle Quit Popup / Quit",
    //         ]
    //         .join(" ");
    //         let footer = Paragraph::new(footer_text)
    //             .alignment(Alignment::Center)
    //             .block(Block::default().borders(Borders::TOP));
    //         f.render_widget(footer, chunks[2]);
    //
    //         if app.show_popup {
    //             // Render normal viewer behind popup if desired.
    //             let main_chunks = Layout::default()
    //                 .direction(Direction::Horizontal)
    //                 .constraints(
    //                     [Constraint::Length(app.sidebar_width), Constraint::Min(0)].as_ref(),
    //                 )
    //                 .split(chunks[1]);
    //
    //             let viewer = Paragraph::new(app.viewer_text.as_str())
    //                 .block(Block::default().borders(Borders::ALL).title("Viewer"));
    //             f.render_widget(viewer, main_chunks[1]);
    //
    //             // Render popup (centered) with the sidebar list.
    //             let popup_area = centered_rect(60, 50, chunks[1]);
    //             // Clear background behind popup.
    //             f.render_widget(ratatui::widgets::Clear, popup_area);
    //             let items: Vec<ListItem> = app
    //                 .sidebar_items
    //                 .iter()
    //                 .map(|i| ListItem::new(i.as_str()))
    //                 .collect();
    //             let mut state = ListState::default();
    //             state.select(Some(app.selected));
    //             let popup_list = List::new(items)
    //                 .block(
    //                     Block::default()
    //                         .borders(Borders::ALL)
    //                         .title("Sidebar (Popup)"),
    //                 )
    //                 .highlight_style(
    //                     Style::default()
    //                         .bg(Color::Blue)
    //                         .fg(Color::White)
    //                         .add_modifier(Modifier::BOLD),
    //                 )
    //                 .highlight_symbol(">> ");
    //             f.render_stateful_widget(popup_list, popup_area, &mut state);
    //         } else {
    //             // Normal layout: sidebar on left and viewer on right.
    //             let main_chunks = Layout::default()
    //                 .direction(Direction::Horizontal)
    //                 .constraints(
    //                     [Constraint::Length(app.sidebar_width), Constraint::Min(0)].as_ref(),
    //                 )
    //                 .split(chunks[1]);
    //
    //             let items: Vec<ListItem> = app
    //                 .sidebar_items
    //                 .iter()
    //                 .map(|i| ListItem::new(i.as_str()))
    //                 .collect();
    //             let mut state = ListState::default();
    //             state.select(Some(app.selected));
    //             let sidebar = List::new(items)
    //                 .block(Block::default().borders(Borders::ALL).title("Sidebar"))
    //                 .highlight_style(
    //                     Style::default()
    //                         .bg(Color::Blue)
    //                         .fg(Color::White)
    //                         .add_modifier(Modifier::BOLD),
    //                 )
    //                 .highlight_symbol(">> ");
    //             f.render_stateful_widget(sidebar, main_chunks[0], &mut state);
    //
    //             let viewer = Paragraph::new(app.viewer_text.as_str())
    //                 .block(Block::default().borders(Borders::ALL).title("Viewer"));
    //             f.render_widget(viewer, main_chunks[1]);
    //         }
    //     })?;
    //
    //     // Poll for events.
    //     let timeout = tick_rate
    //         .checked_sub(last_tick.elapsed())
    //         .unwrap_or_else(|| Duration::from_secs(0));
    //
    //     if crossterm::event::poll(timeout)? {
    //         if let Event::Key(key) = event::read()? {
    //             match key.code {
    //                 // Toggle popup: if popup not shown, press q to show it;
    //                 // if already shown, pressing q quits the app.
    //                 KeyCode::Char('q') => {
    //                     if app.show_popup {
    //                         break 'main_loop;
    //                     } else {
    //                         app.show_popup = true;
    //                     }
    //                 }
    //                 KeyCode::Left => app.resize_sidebar_left(),
    //                 KeyCode::Right => app.resize_sidebar_right(),
    //                 KeyCode::Up => {
    //                     if app.show_popup {
    //                         if app.selected > 0 {
    //                             app.selected -= 1;
    //                         }
    //                     } else {
    //                         app.previous();
    //                     }
    //                 }
    //                 KeyCode::Down => {
    //                     if app.show_popup {
    //                         if app.selected < app.sidebar_items.len() - 1 {
    //                             app.selected += 1;
    //                         }
    //                     } else {
    //                         app.next();
    //                     }
    //                 }
    //                 KeyCode::Char('r') => {
    //                     app.viewer_text = "Restart triggered".into();
    //                 }
    //                 KeyCode::Char('R') => {
    //                     app.viewer_text = "Restart All triggered".into();
    //                 }
    //                 KeyCode::Char('d') => {
    //                     app.viewer_text = "Disable triggered".into();
    //                 }
    //                 _ => {}
    //             }
    //         }
    //     }
    //     if last_tick.elapsed() >= tick_rate {
    //         last_tick = Instant::now();
    //     }
    // }
    //
    // // Restore terminal.
    // disable_raw_mode()?;
    // execute!(
    //     terminal.backend_mut(),
    //     LeaveAlternateScreen,
    //     DisableMouseCapture
    // )?;
    // terminal.show_cursor()?;
    // Ok(())
}

// pub fn render1() -> eyre::Result<()> {
//     // Setup terminal.
//     enable_raw_mode()?;
//     let mut stdout = io::stdout();
//     execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
//     let backend = CrosstermBackend::new(stdout);
//     let mut terminal = Terminal::new(backend)?;
//
//     // Create the application state.
//     let mut app = App::new();
//     let tick_rate = Duration::from_millis(200);
//     let mut last_tick = Instant::now();
//
//     loop {
//         terminal.draw(|f| {
//             // Divide the screen vertically: main area and footer.
//             let vertical_chunks = Layout::default()
//                 .direction(Direction::Vertical)
//                 .constraints([Constraint::Min(0), Constraint::Length(3)].as_ref())
//                 .split(f.area());
//
//             // Divide the main area horizontally: sidebar and viewer.
//             let horizontal_chunks = Layout::default()
//                 .direction(Direction::Horizontal)
//                 .constraints([Constraint::Length(30), Constraint::Min(0)].as_ref())
//                 .split(vertical_chunks[0]);
//
//             // Sidebar: Create list items.
//             let items: Vec<ListItem> = app
//                 .sidebar_items
//                 .iter()
//                 .map(|i| ListItem::new(i.as_str()))
//                 .collect();
//
//             // Prepare a ListState to track the selected item.
//             let mut state = ListState::default();
//             state.select(Some(app.selected));
//
//             let sidebar = List::new(items)
//                 .block(Block::default().borders(Borders::ALL).title("Sidebar"))
//                 .highlight_style(
//                     Style::default()
//                         .bg(Color::Blue)
//                         .fg(Color::White)
//                         .add_modifier(Modifier::BOLD),
//                 )
//                 .highlight_symbol(">> ");
//
//             f.render_stateful_widget(sidebar, horizontal_chunks[0], &mut state);
//
//             // Viewer: Show some text.
//             let viewer = Paragraph::new(app.viewer_text.as_str())
//                 .block(Block::default().borders(Borders::ALL).title("Viewer"));
//             f.render_widget(viewer, horizontal_chunks[1]);
//
//             // Footer: Show available commands.
//             let footer_text = "Commands: ↑/↓ to navigate, q to quit";
//             let footer = Paragraph::new(footer_text).block(Block::default().borders(Borders::ALL));
//             f.render_widget(footer, vertical_chunks[1]);
//         })?;
//
//         let timeout = tick_rate
//             .checked_sub(last_tick.elapsed())
//             .unwrap_or_else(|| Duration::from_secs(0));
//         if crossterm::event::poll(timeout)? {
//             if let Event::Key(key) = event::read()? {
//                 match key.code {
//                     KeyCode::Char('q') => break,
//                     KeyCode::Down => app.next(),
//                     KeyCode::Up => app.previous(),
//                     _ => {}
//                 }
//             }
//         }
//         if last_tick.elapsed() >= tick_rate {
//             last_tick = Instant::now();
//         }
//     }
//
//     // Restore terminal.
//     disable_raw_mode()?;
//     execute!(
//         terminal.backend_mut(),
//         LeaveAlternateScreen,
//         DisableMouseCapture
//     )?;
//     terminal.show_cursor()?;
//     Ok(())
// }
