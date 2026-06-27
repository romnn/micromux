use crate::App;

use ansi_to_tui::IntoText;
use color_eyre::eyre;
use itertools::intersperse;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    prelude::*,
    style::{Color, Modifier, Style, Styled, palette::tailwind},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, Widget, Wrap},
};

fn rendered_line_count(paragraph: &Paragraph<'_>, width: u16) -> u16 {
    let height = paragraph.line_count(width);
    u16::try_from(height).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::{log_view::LogView, rendered_line_count};
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        widgets::{Paragraph, Wrap},
    };
    use similar_asserts::assert_eq;

    fn wrapped_text_height(text: ratatui::text::Text, wrap_width: u16) -> u16 {
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
        rendered_line_count(&paragraph, wrap_width)
    }

    fn count_thumb(buf: &Buffer, area: Rect) -> usize {
        let mut n = 0;
        for y in area.y..area.y.saturating_add(area.height) {
            for x in area.x..area.x.saturating_add(area.width) {
                if buf.cell((x, y)).map(ratatui::buffer::Cell::symbol) == Some("▐") {
                    n += 1;
                }
            }
        }
        n
    }

    fn has_thumb_at(buf: &Buffer, x: u16, y: u16) -> bool {
        buf.cell((x, y)).map(ratatui::buffer::Cell::symbol) == Some("▐")
    }

    fn row_text(buf: &Buffer, x: u16, y: u16, width: u16) -> String {
        let mut out = String::new();
        for col in x..x.saturating_add(width) {
            if let Some(cell) = buf.cell((col, y)) {
                out.push_str(cell.symbol());
            }
        }
        out
    }

    #[test]
    fn wrapped_text_height_matches_expected_rows() {
        let text: ratatui::text::Text = "abcdefghij".into();
        assert_eq!(wrapped_text_height(text.clone(), 4), 3);
        assert_eq!(wrapped_text_height(text.clone(), 5), 2);
        assert_eq!(wrapped_text_height(text, 10), 1);
    }

    #[test]
    fn wrapped_text_height_uses_word_boundaries() {
        let text: ratatui::text::Text = "aaaaaa aaaaaa aaaaaa".into();
        assert_eq!(wrapped_text_height(text, 10), 3);
    }

    #[test]
    fn wrapped_text_height_matches_zero_width_paragraph() {
        let text: ratatui::text::Text = "abcdefghij".into();
        assert_eq!(wrapped_text_height(text, 0), 0);
    }

    #[test]
    fn scrollbar_thumb_is_full_height_when_content_fits() {
        let mut view = LogView {
            follow_tail: false,
            ..LogView::default()
        };

        let buf_area = Rect {
            x: 0,
            y: 0,
            width: 12,
            height: 7,
        };
        let mut buf = Buffer::empty(buf_area);

        let log_area = Rect {
            x: 0,
            y: 0,
            width: 11,
            height: 7,
        };
        let scrollbar_area = Rect {
            x: 11,
            y: 1,
            width: 1,
            height: 5,
        };

        view.render(log_area, scrollbar_area, 0, "one line", &mut buf);

        assert_eq!(
            count_thumb(&buf, scrollbar_area),
            scrollbar_area.height as usize
        );
    }

    #[test]
    fn scrollbar_thumb_moves_to_bottom_when_following_tail() {
        let mut view = LogView::default();

        let buf_area = Rect {
            x: 0,
            y: 0,
            width: 12,
            height: 7,
        };
        let mut buf = Buffer::empty(buf_area);

        let log_area = Rect {
            x: 0,
            y: 0,
            width: 11,
            height: 7,
        };
        let scrollbar_area = Rect {
            x: 11,
            y: 1,
            width: 1,
            height: 5,
        };

        let logs = (0..50)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        view.render(log_area, scrollbar_area, 0, &logs, &mut buf);

        assert!(count_thumb(&buf, scrollbar_area) < scrollbar_area.height as usize);
        assert!(has_thumb_at(
            &buf,
            scrollbar_area.x,
            scrollbar_area.y + scrollbar_area.height - 1
        ));
    }

    #[test]
    fn wrap_changes_scrollbar_behavior_for_long_lines() {
        let mut view = LogView::default();

        let buf_area = Rect {
            x: 0,
            y: 0,
            width: 12,
            height: 7,
        };

        let log_area = Rect {
            x: 0,
            y: 0,
            width: 11,
            height: 7,
        };
        let scrollbar_area = Rect {
            x: 11,
            y: 1,
            width: 1,
            height: 5,
        };

        let logs = "0123456789012345678901234567890123456789";

        let mut buf1 = Buffer::empty(buf_area);
        view.wrap = false;
        view.render(log_area, scrollbar_area, 0, logs, &mut buf1);
        let thumb_unwrapped = count_thumb(&buf1, scrollbar_area);

        let mut buf2 = Buffer::empty(buf_area);
        view.wrap = true;
        view.render(log_area, scrollbar_area, 0, logs, &mut buf2);
        let thumb_wrapped = count_thumb(&buf2, scrollbar_area);

        assert!(thumb_wrapped <= thumb_unwrapped);
    }

    #[test]
    fn wrapped_follow_tail_reaches_final_rendered_row() {
        let mut view = LogView {
            wrap: true,
            follow_tail: true,
            ..LogView::default()
        };

        let buf_area = Rect {
            x: 0,
            y: 0,
            width: 9,
            height: 4,
        };
        let mut buf = Buffer::empty(buf_area);
        let log_area = Rect {
            x: 0,
            y: 0,
            width: 8,
            height: 4,
        };
        let scrollbar_area = Rect {
            x: 8,
            y: 1,
            width: 1,
            height: 2,
        };

        let rendered = view.render(
            log_area,
            scrollbar_area,
            0,
            "abcdefghijklmnopqrstuvwx",
            &mut buf,
        );

        assert_eq!(rendered, 4);
        assert_eq!(view.scroll_offset, 2);
        assert_eq!(row_text(&buf, 1, 1, 6), "mnopqr");
        assert_eq!(row_text(&buf, 1, 2, 6), "stuvwx");
    }
}

#[must_use]
pub fn state_name(service: crate::state::Execution) -> &'static str {
    match service {
        crate::state::Execution::Running {
            health: Some(health),
        } => health.into(),
        state => state.into(),
    }
}

impl Widget for &mut App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let [header_area, main_area, footer_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // header
                Constraint::Min(0),    // main area
                Constraint::Length(1), // footer
            ])
            .spacing(0)
            .areas(area);

        let [services_area, main_right_area] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(self.state.services_sidebar_width),
                Constraint::Min(0),
            ])
            .spacing(0)
            .areas(main_area);

        let [logs_area, health_area] = if self.show_healthcheck_pane {
            let [a, b] = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .spacing(0)
                .areas::<2>(main_right_area);
            [a, b]
        } else {
            [main_right_area, Rect::default()]
        };

        let header = format!("micromux v{}", env!("CARGO_PKG_VERSION"))
            .bold()
            .fg(App::HEADER_COLOR)
            .into_centered_line();
        Paragraph::new(header).render(header_area, buf);
        self.render_services(services_area, buf);
        self.render_logs(logs_area, buf);
        if self.show_healthcheck_pane {
            self.render_healthchecks(health_area, buf);
        }
        self.render_footer(footer_area, buf);
    }
}

impl App {
    const HEADER_COLOR: Color = tailwind::YELLOW.c500;
    const HIGHLIGHT_COLOR: Color = tailwind::GRAY.c900;

    fn render_services(&self, area: Rect, buf: &mut Buffer) {
        let items: Vec<ListItem> = self
            .state
            .services
            .iter()
            .map(|(_name, service)| {
                let status = format!("{: >10}", state_name(service.exec_state))
                    .set_style(crate::style::service_style(service.exec_state));

                // Combine into one line.
                let ports = service
                    .open_ports
                    .iter()
                    .map(|i| format!(":{i}").fg(tailwind::GRAY.c400));

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

                ListItem::new(line.collect::<Line>())
            })
            .collect();

        let mut state = ListState::default();
        state.select(Some(self.state.selected_service));

        let sidebar = List::new(items)
            .block(Block::default().borders(Borders::ALL).title("Services"))
            .highlight_style(
                Style::default()
                    .bg(Self::HIGHLIGHT_COLOR)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(" > ");
        StatefulWidget::render(&sidebar, area, buf, &mut state);
    }

    fn render_logs(&mut self, area: Rect, buf: &mut Buffer) {
        let Some(current_service) = self.state.current_service_mut() else {
            return;
        };
        if current_service.logs_dirty {
            let (num_lines, logs) = current_service.logs.full_text();
            current_service.cached_num_lines = num_lines;
            current_service.cached_logs = logs;
            current_service.logs_dirty = false;
        }

        let num_lines = current_service.cached_num_lines;
        let current_logs = current_service.cached_logs.as_str();
        tracing::trace!(service_id = current_service.id, num_lines, "collected logs");

        // Split into a main pane and a thin scrollbar pane
        let [logs_area, scrollbar_area] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),    // logs view
                Constraint::Length(1), // scrollbar
            ])
            .spacing(0)
            .areas(area);

        let scrollbar_area = Rect {
            x: scrollbar_area.x,
            y: scrollbar_area.y.saturating_add(1),
            width: scrollbar_area.width,
            height: scrollbar_area.height.saturating_sub(2),
        };

        let rendered_lines =
            self.log_view
                .render(logs_area, scrollbar_area, num_lines, current_logs, buf);

        // Persist the wrap-aware rendered line count so keyboard scrolling (scroll_logs_down)
        // clamps to the same bottom the scrollbar uses, even when wrapping is enabled.
        if let Some(current_service) = self.state.current_service_mut() {
            current_service.cached_num_lines = rendered_lines;
        }
    }

    fn render_healthchecks(&mut self, area: Rect, buf: &mut Buffer) {
        let Some(current_service) = self.state.current_service_mut() else {
            return;
        };
        if current_service.healthcheck_dirty {
            let mut out = String::new();

            if !current_service.healthcheck_configured {
                out.push_str("no healthcheck configured");
            } else if current_service.healthcheck_attempts.is_empty() {
                out.push_str("healthcheck pending");
            } else {
                for (idx, attempt) in current_service.healthcheck_attempts.iter().enumerate() {
                    if idx > 0 {
                        out.push('\n');
                    }

                    let (success, exit_code) = attempt
                        .result
                        .map_or((None, None), |r| (Some(r.success), Some(r.exit_code)));

                    // Separator line rendered with ANSI so ansi_to_tui can color it reliably.
                    let status = match (success, exit_code) {
                        (Some(true), Some(code)) => {
                            format!("\x1b[32m[healthcheck ok exit_code={code}]\x1b[0m")
                        }
                        (Some(false), Some(code)) => {
                            format!("\x1b[31m[healthcheck failed exit_code={code}]\x1b[0m")
                        }
                        _ => "\x1b[33m[healthcheck running]\x1b[0m".to_string(),
                    };

                    out.push_str(&status);
                    if !attempt.command.is_empty() {
                        out.push(' ');
                        out.push_str(&attempt.command);
                    }
                    out.push('\n');
                    out.push('\n');

                    let attempt_text = attempt.output.full_text();
                    if !attempt_text.is_empty() {
                        out.push_str(&attempt_text);
                    }
                }
            }

            current_service.healthcheck_cached_text = out;
            current_service.healthcheck_dirty = false;
        }

        let text = current_service.healthcheck_cached_text.as_str();

        let [pane_area, scrollbar_area] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .spacing(0)
            .areas(area);

        let scrollbar_area = Rect {
            x: scrollbar_area.x,
            y: scrollbar_area.y.saturating_add(1),
            width: scrollbar_area.width,
            height: scrollbar_area.height.saturating_sub(2),
        };

        let tui_text: ratatui::text::Text = text.into_text().unwrap_or_else(|err| {
            let escaped = strip_ansi_escapes::strip_str(text);
            tracing::error!(?err, escaped, "failed to sanitize healthcheck output");
            escaped.into()
        });

        let raw_num_lines = u16::try_from(tui_text.height()).unwrap_or(u16::MAX);
        let wrap_width = pane_area.width.saturating_sub(2);
        let mut paragraph = Paragraph::new(tui_text);
        if self.healthcheck_view.wrap {
            paragraph = paragraph.wrap(Wrap { trim: false });
        }
        let num_lines: u16 = if self.healthcheck_view.wrap {
            rendered_line_count(&paragraph, wrap_width)
        } else {
            raw_num_lines
        };
        current_service.healthcheck_cached_num_lines = num_lines;

        let viewport_height = scrollbar_area.height;
        let max_off = num_lines.saturating_sub(viewport_height);
        if self.healthcheck_view.follow_tail {
            self.healthcheck_view.scroll_offset = max_off;
        } else {
            self.healthcheck_view.scroll_offset = self.healthcheck_view.scroll_offset.min(max_off);
        }

        let content_length = (max_off as usize).saturating_add(1).max(1);
        self.healthcheck_view.scrollbar_state = self
            .healthcheck_view
            .scrollbar_state
            .content_length(content_length)
            .viewport_content_length(viewport_height.into())
            .position(self.healthcheck_view.scroll_offset as usize);

        let paragraph = paragraph
            .block(Block::default().borders(Borders::ALL).title("Healthcheck"))
            .scroll((self.healthcheck_view.scroll_offset, 0));
        Widget::render(&paragraph, pane_area, buf);

        let scrollbar = Scrollbar::new(ratatui::widgets::ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(None)
            .thumb_symbol("▐");
        StatefulWidget::render(
            scrollbar,
            scrollbar_area,
            buf,
            &mut self.healthcheck_view.scrollbar_state,
        );
    }

    fn render_footer(&self, area: Rect, buf: &mut Buffer) {
        #[derive(Debug)]
        struct Keys<'a> {
            keys: &'a str,
            description: String,
        }

        impl<'a> Keys<'a> {
            fn new(keys: &'a str, description: impl Into<String>) -> Self {
                Self {
                    keys,
                    description: description.into(),
                }
            }
        }

        let tail = if self.log_view.follow_tail {
            "ON"
        } else {
            "OFF"
        };
        let wrap = if self.log_view.wrap { "ON" } else { "OFF" };
        let attach = if self.attach_mode { "ON" } else { "OFF" };
        let focus = match self.focus {
            crate::Focus::Services => "SERVICES",
            crate::Focus::Logs => "LOGS",
            crate::Focus::Healthcheck => "HEALTH",
        };

        let footer_text = [
            Keys::new("↑/↓", "Navigate"),
            Keys::new("←/→", "Resize"),
            Keys::new("Tab", format!("Focus:{focus}")),
            Keys::new("a", format!("Attach:{attach}")),
            Keys::new("Alt+Esc", "Detach"),
            Keys::new("H", "Health"),
            Keys::new("w", format!("Wrap:{wrap}")),
            Keys::new("t", format!("Tail:{tail}")),
            Keys::new("r", "Restart"),
            Keys::new("R", "Restart All"),
            Keys::new("d", "Disable/Enable"),
            Keys::new("q", "Quit"),
        ];

        let widget = Paragraph::new(
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
        .wrap(ratatui::widgets::Wrap { trim: false });

        Widget::render(&widget, area, buf);
    }

    /// Run the application in the terminal.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The terminal backend fails to initialize or restore.
    /// - The underlying event loop (`App::run`) fails.
    pub async fn render(self) -> eyre::Result<()> {
        let terminal = ratatui::init();
        // Always restore the terminal, even when the event loop returns an error, so a failure
        // never leaves the user's shell stuck in raw mode / the alternate screen.
        let result = self.run(terminal).await;
        ratatui::restore();
        result
    }
}

pub mod log_view {
    use ansi_to_tui::IntoText;
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        widgets::{
            Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarState, StatefulWidget, Widget,
            Wrap,
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
        /// Render the log view and return the wrap-aware rendered line count, so callers can
        /// clamp keyboard scrolling consistently with the scrollbar/follow-tail behavior.
        pub fn render(
            &mut self,
            log_area: Rect,
            scrollbar_area: Rect,
            _num_lines: u16,
            logs: &str,
            buf: &mut Buffer,
        ) -> u16 {
            Clear.render(log_area, buf);
            Clear.render(scrollbar_area, buf);

            // Strip ANSI control codes that could confuse our TUI
            let text: ratatui::text::Text = logs.into_text().unwrap_or_else(|err| {
                // As a fallback, remove all ANSI controls (losing all color)
                let escaped = strip_ansi_escapes::strip_str(logs);
                tracing::error!(?err, escaped, "failed to sanitize log line");
                escaped.into()
            });

            let raw_num_lines = u16::try_from(text.height()).unwrap_or(u16::MAX);
            let wrap_width = log_area.width.saturating_sub(2);
            let mut paragraph = Paragraph::new(text);
            if self.wrap {
                paragraph = paragraph.wrap(Wrap { trim: false });
            }
            let num_lines = if self.wrap {
                super::rendered_line_count(&paragraph, wrap_width)
            } else {
                raw_num_lines
            };

            let viewport_height = scrollbar_area.height;
            let max_off = num_lines.saturating_sub(viewport_height);

            if self.follow_tail {
                self.scroll_offset = max_off;
            } else {
                self.scroll_offset = self.scroll_offset.min(max_off);
            }

            let content_length = (max_off as usize).saturating_add(1).max(1);
            self.scrollbar_state = self
                .scrollbar_state
                .content_length(content_length)
                .viewport_content_length(viewport_height.into())
                .position(self.scroll_offset as usize);

            let paragraph = paragraph
                .block(Block::default().borders(Borders::ALL).title("Logs"))
                .scroll((self.scroll_offset, 0)); // scroll by lines then cols

            Widget::render(&paragraph, log_area, buf);

            let scrollbar = Scrollbar::new(ratatui::widgets::ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(None)
                .thumb_symbol("▐");

            StatefulWidget::render(scrollbar, scrollbar_area, buf, &mut self.scrollbar_state);

            num_lines
        }
    }
}
