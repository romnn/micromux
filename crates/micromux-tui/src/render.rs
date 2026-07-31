use crate::App;

use ansi_to_tui::IntoText;
use itertools::intersperse;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    prelude::*,
    style::{Color, Modifier, Style, Styled, palette::tailwind},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Widget},
};

#[cfg(test)]
mod tests {
    use super::{
        lease_phrase,
        log_view::{LogView, RenderedLineIndex, window_text},
        service_detail_line, shell_join, state_name,
    };
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        text::{Line, Span, Text},
    };
    use similar_asserts::assert_eq;

    fn wrapped_text_height(text: &ratatui::text::Text, wrap_width: u16) -> usize {
        let mut index = RenderedLineIndex::default();
        index.rebuild(text, true, wrap_width);
        index.total_lines()
    }

    fn render_logs(
        view: &mut LogView,
        log_area: Rect,
        scrollbar_area: Rect,
        logs: &str,
        buf: &mut Buffer,
    ) -> usize {
        let text = Text::from(logs.to_string());
        let mut index = RenderedLineIndex::default();
        index.rebuild(&text, view.wrap, log_area.width.saturating_sub(2));
        let area = Rect {
            x: log_area.x,
            y: log_area.y,
            width: log_area.width.saturating_add(scrollbar_area.width),
            height: log_area.height,
        };
        view.render(area, &index, &text, "Logs", None, buf)
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
        assert_eq!(wrapped_text_height(&text, 4), 3);
        assert_eq!(wrapped_text_height(&text, 5), 2);
        assert_eq!(wrapped_text_height(&text, 10), 1);
    }

    #[test]
    fn retired_state_takes_precedence_over_disabled_state() {
        let mut snapshot = micromux::ServiceSnapshot::initial(
            "removed".to_string(),
            "removed".to_string(),
            Vec::new(),
            None,
            micromux::RestartPolicy::Never,
            Vec::new(),
            None,
        );
        snapshot.desired = micromux::Desired::Disabled;
        snapshot.retired = Some(micromux::RetiredReason::Removed);

        assert_eq!(state_name(&snapshot), "RETIRED");
    }

    #[test]
    fn shell_join_quotes_only_arguments_a_shell_would_split() {
        let argv = vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo hi".to_string(),
            String::new(),
        ];
        assert_eq!(shell_join(&argv), r#"sh -c "echo hi" """#);
    }

    #[test]
    fn detail_command_is_capped_so_bounded_facts_survive_pathological_commands() {
        let mut snapshot = micromux::ServiceSnapshot::initial(
            "svc".to_string(),
            "svc".to_string(),
            Vec::new(),
            None,
            micromux::RestartPolicy::Never,
            vec!["sh".to_string(), "-c".to_string(), "x".repeat(500)],
            None,
        );
        snapshot.run_generation = 1;

        let line = service_detail_line(&snapshot, 1_000)
            .map(|line| line.to_string())
            .unwrap_or_default();

        assert!(line.contains('…'));
        assert!(line.ends_with(" gen 1 "));
        assert!(line.chars().count() < 100);
    }

    #[test]
    fn lease_phrase_covers_every_magnitude_and_the_unbounded_lease() {
        assert_eq!(lease_phrase(None, 1_000), "no expiry");
        assert_eq!(lease_phrase(Some(500), 1_000), "expired");
        assert_eq!(lease_phrase(Some(31_000), 1_000), "expires in ~30s");
        assert_eq!(lease_phrase(Some(91_000), 1_000), "expires in ~1m");
        assert_eq!(lease_phrase(Some(7_201_000), 1_000), "expires in ~2h");
        assert_eq!(lease_phrase(Some(259_201_000), 1_000), "expires in ~3d");
    }

    #[test]
    fn service_detail_shows_the_command_generation_and_dynamic_lease_facts() {
        let mut snapshot = micromux::ServiceSnapshot::initial(
            "svc".to_string(),
            "svc".to_string(),
            Vec::new(),
            None,
            micromux::RestartPolicy::Never,
            vec!["sh".to_string(), "-c".to_string(), "sleep 60".to_string()],
            None,
        );
        snapshot.run_generation = 3;
        let configured = service_detail_line(&snapshot, 1_000)
            .map(|line| line.to_string())
            .unwrap_or_default();
        assert_eq!(configured, r#" $ sh -c "sleep 60"  gen 3 "#);

        snapshot.origin = micromux::OriginKind::Dynamic;
        snapshot.dynamic = Some(micromux::DynamicServiceInfo {
            created_at_unix_ms: 0,
            expires_at_unix_ms: Some(61_000),
            owner: Some("agent".to_string()),
            revision: 2,
        });
        let dynamic = service_detail_line(&snapshot, 1_000)
            .map(|line| line.to_string())
            .unwrap_or_default();
        assert_eq!(
            dynamic,
            r#" $ sh -c "sleep 60"  gen 3  dynamic · rev 2 · expires in ~1m · owner agent "#
        );

        // A countdown on a dead lease would only mislead; retirement owns the status column.
        snapshot.retired = Some(micromux::RetiredReason::Expired);
        let retired = service_detail_line(&snapshot, 1_000)
            .map(|line| line.to_string())
            .unwrap_or_default();
        assert_eq!(
            retired,
            r#" $ sh -c "sleep 60"  gen 3  dynamic · rev 2 · owner agent "#
        );
    }

    #[test]
    fn wrapped_text_height_uses_word_boundaries() {
        let text: ratatui::text::Text = "aaaaaa aaaaaa aaaaaa".into();
        assert_eq!(wrapped_text_height(&text, 10), 3);
    }

    #[test]
    fn wrapped_text_height_matches_zero_width_paragraph() {
        let text: ratatui::text::Text = "abcdefghij".into();
        assert_eq!(wrapped_text_height(&text, 0), 0);
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

        render_logs(&mut view, log_area, scrollbar_area, "one line", &mut buf);

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
        render_logs(&mut view, log_area, scrollbar_area, &logs, &mut buf);

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
        render_logs(&mut view, log_area, scrollbar_area, logs, &mut buf1);
        let thumb_unwrapped = count_thumb(&buf1, scrollbar_area);

        let mut buf2 = Buffer::empty(buf_area);
        view.wrap = true;
        render_logs(&mut view, log_area, scrollbar_area, logs, &mut buf2);
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

        let rendered = render_logs(
            &mut view,
            log_area,
            scrollbar_area,
            "abcdefghijklmnopqrstuvwx",
            &mut buf,
        );

        assert_eq!(rendered, 4);
        assert_eq!(view.scroll_offset, 2);
        assert_eq!(row_text(&buf, 1, 1, 6), "mnopqr");
        assert_eq!(row_text(&buf, 1, 2, 6), "stuvwx");
    }

    #[test]
    fn log_window_avoids_the_paragraph_scroll_limit_for_many_logical_lines() {
        let line_count = usize::from(u16::MAX) + 100;
        let text = Text::from(
            (0..line_count)
                .map(|index| Line::raw(index.to_string()))
                .collect::<Vec<_>>(),
        );

        let mut index = RenderedLineIndex::default();
        index.rebuild(&text, false, 80);
        let (window, local_offset) = window_text(&text, &index, line_count - 1, 1);
        let expected = line_count.saturating_sub(1).to_string();

        assert_eq!(local_offset, 0);
        assert_eq!(
            window
                .lines
                .first()
                .map(std::string::ToString::to_string)
                .as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn log_window_borrows_cached_span_content() {
        let text = Text::from(Line::from(Span::raw("cached".to_string())));
        let original = text
            .lines
            .first()
            .and_then(|line| line.spans.first())
            .map(|span| span.content.as_ptr());

        let mut index = RenderedLineIndex::default();
        index.rebuild(&text, false, 80);
        let (window, _) = window_text(&text, &index, 0, 1);
        let borrowed = window
            .lines
            .first()
            .and_then(|line| line.spans.first())
            .map(|span| span.content.as_ptr());

        assert_eq!(borrowed, original);
    }

    #[test]
    fn log_window_borrows_only_the_visible_source_lines() {
        let text = Text::from(
            (0..1_000)
                .map(|index| Line::raw(index.to_string()))
                .collect::<Vec<_>>(),
        );
        let mut index = RenderedLineIndex::default();
        index.rebuild(&text, false, 80);

        let (window, local_offset) = window_text(&text, &index, 900, 10);

        assert_eq!(local_offset, 0);
        assert_eq!(window.lines.len(), 10);
        assert_eq!(
            window.lines.first().map(Line::to_string).as_deref(),
            Some("900")
        );
        assert_eq!(
            window.lines.last().map(Line::to_string).as_deref(),
            Some("909")
        );
    }

    #[test]
    fn healthcheck_text_matches_the_model_format() {
        use super::build_healthcheck_text;
        use micromux::{HealthAttempt, HealthLine, HealthResult, OutputStream};

        assert_eq!(
            build_healthcheck_text(false, &[]),
            "no healthcheck configured"
        );
        assert_eq!(build_healthcheck_text(true, &[]), "healthcheck pending");

        let ok = HealthAttempt {
            run_generation: 1,
            attempt: 1,
            command: "curl -f localhost".to_string(),
            output: vec![
                HealthLine {
                    stream: OutputStream::Stdout,
                    line: "ok".to_string(),
                },
                HealthLine {
                    stream: OutputStream::Stderr,
                    line: "warn".to_string(),
                },
            ],
            result: Some(HealthResult {
                success: true,
                exit_code: 0,
                cancelled: false,
            }),
        };
        assert_eq!(
            build_healthcheck_text(true, std::slice::from_ref(&ok)),
            "\x1b[32m[healthcheck ok exit_code=0]\x1b[0m curl -f localhost\n\nok\n[stderr] warn"
        );

        let running = HealthAttempt {
            run_generation: 1,
            attempt: 2,
            command: "probe".to_string(),
            output: vec![],
            result: None,
        };
        assert_eq!(
            build_healthcheck_text(true, std::slice::from_ref(&running)),
            "\x1b[33m[healthcheck running]\x1b[0m probe\n\n"
        );

        let cancelled = HealthAttempt {
            run_generation: 1,
            attempt: 3,
            command: "probe".to_string(),
            output: Vec::new(),
            result: Some(HealthResult {
                success: false,
                exit_code: -1,
                cancelled: true,
            }),
        };
        assert_eq!(
            build_healthcheck_text(true, std::slice::from_ref(&cancelled)),
            "\x1b[90m[healthcheck cancelled]\x1b[0m probe\n\n"
        );
    }
}

/// Build the healthcheck pane text from the model's bounded attempt history.
fn build_healthcheck_text(configured: bool, attempts: &[micromux::HealthAttempt]) -> String {
    let mut out = String::new();
    if !configured {
        out.push_str("no healthcheck configured");
        return out;
    }
    if attempts.is_empty() {
        out.push_str("healthcheck pending");
        return out;
    }
    for (idx, attempt) in attempts.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }

        let result = attempt.result;

        // Separator line rendered with ANSI so ansi_to_tui can color it reliably.
        let status = match result {
            Some(result) if result.cancelled => {
                "\x1b[90m[healthcheck cancelled]\x1b[0m".to_string()
            }
            Some(result) if result.success => {
                let code = result.exit_code;
                format!("\x1b[32m[healthcheck ok exit_code={code}]\x1b[0m")
            }
            Some(result) => {
                let code = result.exit_code;
                format!("\x1b[31m[healthcheck failed exit_code={code}]\x1b[0m")
            }
            None => "\x1b[33m[healthcheck running]\x1b[0m".to_string(),
        };

        out.push_str(&status);
        if !attempt.command.is_empty() {
            out.push(' ');
            out.push_str(&attempt.command);
        }
        out.push('\n');
        out.push('\n');

        let attempt_text = attempt
            .output
            .iter()
            .map(|line| match line.stream {
                micromux::OutputStream::Stderr => format!("[stderr] {}", line.line),
                micromux::OutputStream::Stdout | micromux::OutputStream::Unknown => {
                    line.line.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !attempt_text.is_empty() {
            out.push_str(&attempt_text);
        }
    }
    out
}

fn state_name(snapshot: &micromux::ServiceSnapshot) -> &'static str {
    if snapshot.retired.is_some() {
        return "RETIRED";
    }
    if snapshot.desired == micromux::Desired::Disabled {
        return "DISABLED";
    }

    match snapshot.execution {
        micromux::Execution::Pending => "PENDING",
        micromux::Execution::Blocked => "BLOCKED",
        micromux::Execution::Starting => "STARTING",
        micromux::Execution::Running => match snapshot.health {
            Some(micromux::Health::Healthy) => "HEALTHY",
            Some(micromux::Health::Unhealthy) => "UNHEALTHY",
            Some(micromux::Health::Unknown) => "UNKNOWN",
            None => "RUNNING",
        },
        micromux::Execution::Stopping => "KILLED",
        micromux::Execution::Exited => "EXITED",
        micromux::Execution::Unknown => "UNKNOWN",
    }
}

/// Join argv for display, quoting only arguments a shell would split.
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            if arg.is_empty() || arg.chars().any(char::is_whitespace) {
                format!("{arg:?}")
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Longest command rendered in the detail line. An unbounded command would push the bounded
/// facts (generation, revision, lease) off the border; the full command stays available through
/// `ctl ls` and the MCP snapshot.
const DETAIL_COMMAND_MAX_CHARS: usize = 80;

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

/// Wall clock in the unit lease expiries are expressed in.
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

/// Human phrase for a dynamic service's lease. The remaining time is computed at draw time and
/// marked approximate — the TUI redraws on changes, not on a clock.
fn lease_phrase(expires_at_unix_ms: Option<u64>, now_unix_ms: u64) -> String {
    let Some(expires_at_unix_ms) = expires_at_unix_ms else {
        return "no expiry".to_string();
    };
    let Some(remaining_ms) = expires_at_unix_ms.checked_sub(now_unix_ms) else {
        return "expired".to_string();
    };
    let secs = remaining_ms / 1000;
    if secs < 60 {
        format!("expires in ~{secs}s")
    } else if secs < 3600 {
        format!("expires in ~{}m", secs / 60)
    } else if secs < 86_400 {
        format!("expires in ~{}h", secs / 3600)
    } else {
        format!("expires in ~{}d", secs / 86_400)
    }
}

/// One-line identity of the selected service for the logs pane frame: the resolved command it
/// runs, its run generation, and for dynamic services the definition revision plus the lease and
/// ownership facts an operator needs at a glance.
fn service_detail_line(
    snapshot: &micromux::ServiceSnapshot,
    now_unix_ms: u64,
) -> Option<Line<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let command = shell_join(&snapshot.command);
    if !command.is_empty() {
        let command = truncate_chars(&command, DETAIL_COMMAND_MAX_CHARS);
        spans.push(format!(" $ {command} ").fg(tailwind::GRAY.c400));
    }
    spans.push(format!(" gen {} ", snapshot.run_generation).fg(tailwind::GRAY.c400));
    if snapshot.origin == micromux::OriginKind::Dynamic {
        let mut facts = vec!["dynamic".to_string()];
        if let Some(dynamic) = &snapshot.dynamic {
            facts.push(format!("rev {}", dynamic.revision));
            // Retirement already owns the status column; a countdown on a dead lease would only
            // mislead.
            if snapshot.retired.is_none() {
                facts.push(lease_phrase(dynamic.expires_at_unix_ms, now_unix_ms));
            }
            if let Some(owner) = &dynamic.owner {
                facts.push(format!("owner {owner}"));
            }
        }
        spans.push(format!(" {} ", facts.join(" · ")).fg(tailwind::YELLOW.c500));
    }
    (!spans.is_empty()).then(|| Line::from(spans))
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

        let header = self
            .attachment_header()
            .or_else(|| self.local_warning_header())
            .unwrap_or_else(|| {
                format!("micromux v{}", env!("CARGO_PKG_VERSION"))
                    .bold()
                    .fg(App::HEADER_COLOR)
                    .into_centered_line()
            });
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

    fn attachment_header(&self) -> Option<Line<'static>> {
        let status = self.source.attachment_status()?;
        let mut spans = if status.connected {
            vec!["attached: ".fg(tailwind::GREEN.c400).bold()]
        } else {
            vec!["reconnecting… ".fg(tailwind::RED.c400).bold()]
        };
        spans.extend([
            status.session.name.bold(),
            format!(" ({})", status.session.config_path).into(),
        ]);
        if let Some(notice) = status.notice.as_deref().or_else(|| self.terminal_notice()) {
            spans.extend([" — ".into(), notice.to_string().fg(tailwind::RED.c400)]);
        }
        Some(Line::from(spans).centered())
    }

    fn local_warning_header(&self) -> Option<Line<'static>> {
        let notice = self
            .source
            .local_notice()
            .or_else(|| self.terminal_notice())?;
        Some(
            Line::from(vec![
                format!("micromux v{}", env!("CARGO_PKG_VERSION"))
                    .bold()
                    .fg(App::HEADER_COLOR),
                " — WARNING: ".fg(tailwind::RED.c400).bold(),
                notice.to_string().fg(tailwind::RED.c400),
            ])
            .centered(),
        )
    }

    fn terminal_notice(&self) -> Option<&str> {
        self.terminal_input_closed
            .then_some(crate::TERMINAL_INPUT_CLOSED_NOTICE)
            .or(self.input_notice.as_deref())
    }

    fn render_services(&self, area: Rect, buf: &mut Buffer) {
        let items: Vec<ListItem> = self
            .state
            .services
            .iter()
            .map(|service| {
                let status = format!("{: >10}", state_name(&service.snapshot))
                    .set_style(crate::style::service_style(&service.snapshot));

                // Combine into one line.
                let ports = service
                    .snapshot
                    .advertised_ports
                    .iter()
                    .map(|i| format!(":{i}").fg(tailwind::GRAY.c400));

                let origin = match service.snapshot.origin {
                    micromux::OriginKind::Dynamic => "+",
                    micromux::OriginKind::Configured | micromux::OriginKind::Unknown => " ",
                };
                let line = [
                    status,
                    " ".into(),
                    origin.fg(tailwind::GRAY.c400),
                    service.snapshot.id.as_str().into(),
                ]
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
        let Some(current_id) = self
            .state
            .current_service()
            .map(|service| service.snapshot.id.clone())
        else {
            return;
        };
        let dirty = self
            .state
            .current_service()
            .is_some_and(|service| service.logs_dirty);
        if dirty {
            let after = self
                .state
                .current_service()
                .and_then(|service| service.cached_lines.back())
                .map_or(0, |(seq, _)| seq.saturating_sub(1));
            let (first_retained, new_lines) = self.source.logs_since(&current_id, after);
            if let Some(service) = self.state.current_service_mut() {
                match first_retained {
                    None => service.cached_lines.clear(),
                    Some(first) => {
                        while service
                            .cached_lines
                            .front()
                            .is_some_and(|(seq, _)| *seq < first)
                        {
                            service.cached_lines.pop_front();
                        }
                    }
                }
                for line in new_lines {
                    let formatted = crate::json_log::format_line(&line.line, self.pretty_json_logs);
                    match service.cached_lines.back_mut() {
                        Some((seq, cached)) if *seq == line.seq => *cached = formatted,
                        _ => service.cached_lines.push_back((line.seq, formatted)),
                    }
                }
                service.text_dirty = true;
                service.logs_dirty = false;
            }
        }

        let wrap = self.log_view.wrap;
        let wrap_width = area.width.saturating_sub(3);
        if let Some(service) = self.state.current_service_mut()
            && (service.text_dirty || service.cached_wrap != Some((wrap, wrap_width)))
        {
            let joined = service
                .cached_lines
                .iter()
                .map(|(_, line)| line.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            service.cached_text = joined.as_str().into_text().unwrap_or_else(|err| {
                let escaped = strip_ansi_escapes::strip_str(&joined);
                tracing::error!(
                    ?err,
                    input_bytes = joined.len(),
                    "failed to sanitize log buffer"
                );
                escaped.into()
            });
            service
                .cached_line_index
                .rebuild(&service.cached_text, wrap, wrap_width);
            service.cached_wrap = Some((wrap, wrap_width));
            service.text_dirty = false;
        }

        let Some(current_service) = self.state.current_service() else {
            return;
        };
        let text = &current_service.cached_text;
        let detail = service_detail_line(&current_service.snapshot, now_unix_ms());
        tracing::trace!(
            service_id = current_service.snapshot.id,
            num_lines = current_service.cached_line_index.total_lines(),
            "collected logs"
        );

        self.log_view.render(
            area,
            &current_service.cached_line_index,
            text,
            "Logs",
            detail,
            buf,
        );
    }

    fn render_healthchecks(&mut self, area: Rect, buf: &mut Buffer) {
        let Some(current_id) = self
            .state
            .current_service()
            .map(|service| service.snapshot.id.clone())
        else {
            return;
        };
        let (dirty, configured) = self
            .state
            .current_service()
            .map_or((false, false), |service| {
                (
                    service.healthcheck_dirty,
                    service.snapshot.healthcheck_configured,
                )
            });
        if dirty {
            let attempts = self.source.healthchecks(&current_id);
            let out = build_healthcheck_text(configured, &attempts);
            if let Some(service) = self.state.current_service_mut() {
                service.healthcheck_cached_text = out.as_str().into_text().unwrap_or_else(|err| {
                    let escaped = strip_ansi_escapes::strip_str(&out);
                    tracing::error!(
                        ?err,
                        input_bytes = out.len(),
                        "failed to sanitize healthcheck output"
                    );
                    escaped.into()
                });
                service.healthcheck_cached_wrap = None;
                service.healthcheck_dirty = false;
            }
        }

        let wrap = self.healthcheck_view.wrap;
        let wrap_width = area.width.saturating_sub(3);
        if let Some(service) = self.state.current_service_mut()
            && service.healthcheck_cached_wrap != Some((wrap, wrap_width))
        {
            service.healthcheck_cached_line_index.rebuild(
                &service.healthcheck_cached_text,
                wrap,
                wrap_width,
            );
            service.healthcheck_cached_wrap = Some((wrap, wrap_width));
        }

        let Some(service) = self.state.current_service() else {
            return;
        };
        self.healthcheck_view.render(
            area,
            &service.healthcheck_cached_line_index,
            &service.healthcheck_cached_text,
            "Healthcheck",
            None,
            buf,
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
        let pty_input = if self.pty_input_mode { "ON" } else { "OFF" };
        let focus = match self.focus {
            crate::Focus::Services => "SERVICES",
            crate::Focus::Logs => "LOGS",
            crate::Focus::Healthcheck => "HEALTH",
        };

        let mut footer_text = vec![
            Keys::new("↑/↓", "Navigate"),
            Keys::new("←/→", "Resize"),
            Keys::new("Tab", format!("Focus:{focus}")),
        ];
        if self.input.is_some() {
            footer_text.extend([
                Keys::new("a", format!("PTY Input:{pty_input}")),
                Keys::new("Alt+Esc", "Exit input"),
            ]);
        }
        footer_text.extend([
            Keys::new("H", "Health"),
            Keys::new("w", format!("Wrap:{wrap}")),
            Keys::new("t", format!("Tail:{tail}")),
            Keys::new("r", "Restart"),
            Keys::new("R", "Restart All"),
            Keys::new("d", "Disable/Enable"),
            Keys::new("s", "Stop dynamic"),
            Keys::new(
                "q",
                if self.source.attachment_status().is_some() {
                    "Detach"
                } else {
                    "Quit"
                },
            ),
        ]);

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
    ///
    /// - The terminal backend fails to initialize or restore.
    /// - The underlying event loop (`App::run`) fails.
    pub async fn render(self) -> Result<(), crate::Error> {
        use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};

        let terminal = ratatui::try_init()?;
        let mut stdout = std::io::stdout();
        if let Err(err) = crossterm::execute!(stdout, EnableBracketedPaste) {
            ratatui::restore();
            return Err(err.into());
        }
        // Always restore the terminal, even when the event loop returns an error, so a failure
        // never leaves the user's shell stuck in raw mode / the alternate screen.
        let result = self.run(terminal).await;
        let disable_paste = crossterm::execute!(stdout, DisableBracketedPaste);
        ratatui::restore();
        result?;
        disable_paste?;
        Ok(())
    }
}

pub mod log_view {
    use ratatui::{
        buffer::Buffer,
        layout::{Constraint, Direction, Layout, Rect},
        widgets::{
            Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarState, StatefulWidget, Widget,
            Wrap,
        },
    };

    /// Maps logical text lines to their first rendered row for one wrap configuration.
    #[derive(Debug, Default)]
    pub struct RenderedLineIndex {
        starts: Vec<usize>,
        total_lines: usize,
    }

    impl RenderedLineIndex {
        /// Rebuild the row index after the text or wrap configuration changes.
        pub(crate) fn rebuild(&mut self, text: &ratatui::text::Text<'_>, wrap: bool, width: u16) {
            self.starts.clear();
            self.starts.reserve(text.lines.len());
            let mut rendered = 0usize;
            for line in &text.lines {
                self.starts.push(rendered);
                let height = if wrap {
                    Paragraph::new(borrow_line(line))
                        .wrap(Wrap { trim: false })
                        .line_count(width)
                } else {
                    1
                };
                rendered = rendered.saturating_add(height);
            }
            self.total_lines = rendered;
        }

        /// Total rendered rows covered by this index.
        #[must_use]
        pub(crate) fn total_lines(&self) -> usize {
            self.total_lines
        }

        fn source_window(
            &self,
            offset: usize,
            viewport_height: usize,
        ) -> (std::ops::Range<usize>, u16) {
            if self.starts.is_empty() {
                return (0..0, 0);
            }
            let start = self
                .starts
                .partition_point(|line_start| *line_start <= offset)
                .saturating_sub(1);
            let consumed = self.starts.get(start).copied().unwrap_or_default();
            let end_row = offset.saturating_add(viewport_height.max(1));
            let end = self
                .starts
                .partition_point(|line_start| *line_start < end_row)
                .max(start.saturating_add(1))
                .min(self.starts.len());
            (
                start..end,
                u16::try_from(offset.saturating_sub(consumed)).unwrap_or(u16::MAX),
            )
        }
    }

    #[derive(Debug)]
    pub struct LogView {
        /// Number of rendered rows scrolled from the top.
        pub scroll_offset: usize,
        /// Whether rendering keeps the bottom of the text visible.
        pub follow_tail: bool,
        /// Whether long logical lines wrap across rendered rows.
        pub wrap: bool,
        /// Scrollbar state derived during rendering.
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
        /// Render a scrollable text pane and return its wrap-aware rendered line count, so callers
        /// can clamp keyboard scrolling consistently with the scrollbar/follow-tail behavior.
        ///
        /// `line_index` must describe `text` under this view's current wrap configuration.
        /// `detail` is drawn into the bottom border as the selected service's identity line.
        pub fn render(
            &mut self,
            area: Rect,
            line_index: &RenderedLineIndex,
            text: &ratatui::text::Text<'_>,
            title: &'static str,
            detail: Option<ratatui::text::Line<'static>>,
            buf: &mut Buffer,
        ) -> usize {
            let num_lines = line_index.total_lines();
            let [log_area, scrollbar_area] = Layout::default()
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

            Clear.render(log_area, buf);
            Clear.render(scrollbar_area, buf);

            let viewport_height = usize::from(scrollbar_area.height);
            let max_off = num_lines.saturating_sub(viewport_height);

            if self.follow_tail {
                self.scroll_offset = max_off;
            } else {
                self.scroll_offset = self.scroll_offset.min(max_off);
            }

            let content_length = max_off.saturating_add(1).max(1);
            self.scrollbar_state = self
                .scrollbar_state
                .content_length(content_length)
                .viewport_content_length(viewport_height)
                .position(self.scroll_offset);

            let (text, paragraph_offset) =
                window_text(text, line_index, self.scroll_offset, viewport_height);
            let mut paragraph = Paragraph::new(text);
            if self.wrap {
                paragraph = paragraph.wrap(Wrap { trim: false });
            }

            let mut block = Block::default().borders(Borders::ALL).title(title);
            if let Some(detail) = detail {
                block = block.title_bottom(detail);
            }
            let paragraph = paragraph.block(block).scroll((paragraph_offset, 0));

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

    pub(super) fn window_text<'a>(
        text: &'a ratatui::text::Text<'_>,
        line_index: &RenderedLineIndex,
        offset: usize,
        viewport_height: usize,
    ) -> (ratatui::text::Text<'a>, u16) {
        let (source_lines, local_offset) = line_index.source_window(offset, viewport_height);
        (borrow_text_range(text, source_lines), local_offset)
    }

    fn borrow_text_range<'a>(
        text: &'a ratatui::text::Text<'_>,
        source_lines: std::ops::Range<usize>,
    ) -> ratatui::text::Text<'a> {
        ratatui::text::Text {
            alignment: text.alignment,
            style: text.style,
            lines: text
                .lines
                .get(source_lines)
                .unwrap_or_default()
                .iter()
                .map(borrow_line)
                .collect(),
        }
    }

    fn borrow_line<'a>(line: &'a ratatui::text::Line<'_>) -> ratatui::text::Line<'a> {
        ratatui::text::Line {
            style: line.style,
            alignment: line.alignment,
            spans: line
                .spans
                .iter()
                .map(|span| ratatui::text::Span::styled(span.content.as_ref(), span.style))
                .collect(),
        }
    }
}
