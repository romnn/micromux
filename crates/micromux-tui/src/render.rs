use crate::App;

use color_eyre::eyre;
use itertools::intersperse;
use ratatui::{
    DefaultTerminal, Terminal,
    backend::{Backend, CrosstermBackend},
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    prelude::*,
    style::{Color, Modifier, Style, Styled, palette::tailwind},
    text::Span,
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarState, Widget,
    },
};

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

pub fn state_name(service: crate::state::Execution) -> &'static str {
    match service {
        // state @ (state::Service::Disabled
        // | state::Service::Pending
        // | state::Service::Exited { .. }) => state.into(),
        crate::state::Execution::Running {
            health: Some(health),
        } => health.into(),
        state => state.into(),
    }
    // match (&service.state, &service.health) {
    //     (state @ (State::Disabled | State::Pending | State::Exited { .. }), _) => state.into(),
    //     (_, Some(health)) => health.into(),
    //     (state, _) => state.into(),
    // }
}

impl Widget for &mut App {
    fn render(mut self, area: Rect, buf: &mut Buffer) {
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
                Constraint::Length(self.state.services_sidebar_width),
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
            // .mux
            .state
            .services
            .iter()
            // .map(|i| ListItem::new(i.name.as_str()))
            .map(|(name, service)| {
                // .map(|service| {
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
                let status = format!("{: >10}", state_name(service.exec_state))
                    .set_style(crate::style::service_style(service.exec_state));
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
        state.select(Some(self.state.selected_service));

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

    fn render_logs(&mut self, area: Rect, buf: &mut Buffer) {
        let current_service = self.state.current_service();
        let (num_lines, current_logs) = current_service.logs.full_text();
        tracing::trace!(
            service_id = current_service.id,
            current_logs,
            num_lines,
            "collected logs"
        );

        // Split into a main pane and a thin scrollbar pane
        let [logs_area, scrollbar_area] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),    // logs view
                Constraint::Length(1), // scrollbar
            ])
            .spacing(0)
            .areas(area);

        self.log_view
            .render(logs_area, scrollbar_area, num_lines, &current_logs, buf);
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

    pub async fn render(self) -> eyre::Result<()> {
        let terminal = ratatui::init();
        self.run(terminal).await?;
        ratatui::restore();
        Ok(())
    }
}

pub mod log_view {
    use color_eyre::owo_colors::Style;
    use crossterm::event::KeyCode;
    use ratatui::{
        Frame,
        backend::Backend,
        buffer::Buffer,
        layout::Rect,
        widgets::{
            Block, Borders, Paragraph, Scrollbar, ScrollbarState, StatefulWidget, Widget, Wrap,
        },
    };

    #[derive(Debug)]
    pub struct LogView {
        /// How many lines down from the top we’ve scrolled
        pub scroll_offset: u16,
        /// If true, always keep the bottom of the log visible
        pub follow_tail: bool,
        /// Wrap long lines
        pub wrap: bool,
        // Scrollbar state
        pub scrollbar_state: ScrollbarState,
    }

    impl Default for LogView {
        fn default() -> Self {
            Self {
                scroll_offset: 0,
                follow_tail: true,
                wrap: false,
                scrollbar_state: ScrollbarState::default(),
            }
        }
    }

    impl LogView {
        pub fn render(
            &mut self,
            log_area: Rect,
            scrollbar_area: Rect,
            num_lines: u16,
            logs: &str,
            buf: &mut Buffer,
        ) {
            use ansi_to_tui::IntoText;

            // Account for the two borders
            let viewport_height = log_area.height.saturating_sub(2);

            // If following tail, move scroll_offset so bottom is visible
            if self.follow_tail {
                self.scroll_offset = num_lines.saturating_sub(viewport_height);
            }

            // Update scrollbar state
            self.scrollbar_state = self
                .scrollbar_state
                .content_length(num_lines.into())
                .viewport_content_length(viewport_height.into())
                .position(self.scroll_offset.into());

            // Strip ANSI control codes that could confuse our TUI
            let text: ratatui::text::Text = logs.into_text().unwrap_or_else(|err| {
                // As a fallback, remove all ANSI controls (losing all color)
                let escaped = strip_ansi_escapes::strip_str(logs);
                tracing::error!(?err, escaped, "failed to sanitize log line");
                escaped.into()
            });

            // Build paragraph
            let mut paragraph = Paragraph::new(text)
                .block(Block::default().borders(Borders::ALL).title("Logs"))
                .scroll((self.scroll_offset, 0)); // scroll by lines then cols

            if self.wrap {
                paragraph = paragraph.wrap(Wrap { trim: false });
            }

            Widget::render(&paragraph, log_area, buf);

            let scrollbar = Scrollbar::new(ratatui::widgets::ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_symbol("▐");

            StatefulWidget::render(scrollbar, scrollbar_area, buf, &mut self.scrollbar_state);
        }
    }
}
