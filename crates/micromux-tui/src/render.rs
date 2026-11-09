use crate::App;

use ansi_to_tui::IntoText;
use itertools::intersperse;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    prelude::*,
    style::{Color, Modifier, Style, Styled},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Widget},
};

#[cfg(test)]
mod tests {
    use super::{
        command_rows, lease_phrase,
        log_view::{Highlights, LogView, PaneBorders, RenderedLineIndex, window_text},
        service_detail, short_local_time, state_name,
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
        view.render(
            area,
            &index,
            &text,
            PaneBorders {
                title: Line::raw("Logs"),
                status: None,
                detail: None,
                focused: false,
            },
            Highlights::default(),
            buf,
        )
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
    fn key_hints_wrap_whole_hints_within_the_width() {
        use super::{KeyHint, pack_key_hints};

        let hints = [
            KeyHint::new("?", "Help"),
            KeyHint::toggle("Tab", "Focus", "SERVICES"),
            KeyHint::new("r", "Restart"),
        ];

        let one_row = pack_key_hints(&hints, 80);
        let wrapped = pack_key_hints(&hints, 24);
        let rows = |rows: &[super::HintRow]| {
            rows.iter()
                .map(|row| row.line.to_string())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            rows(&one_row),
            vec![" ? Help  Tab Focus:SERVICES  r Restart"]
        );
        // Each hint moves to the next row whole instead of splitting between key and label.
        assert_eq!(
            rows(&wrapped),
            vec![" ? Help", " Tab Focus:SERVICES", " r Restart"]
        );
        assert!(wrapped.iter().all(|row| row.line.width() <= 24));

        // Each hint's place covers exactly its own text, which a click on it runs.
        let hint_texts = |row: &super::HintRow| {
            let text = row.line.to_string();
            row.hints
                .iter()
                .map(|&(x, width, keys)| {
                    let shown = text.chars().skip(x).take(width).collect::<String>();
                    (shown, keys)
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            one_row.iter().flat_map(hint_texts).collect::<Vec<_>>(),
            vec![
                ("? Help".to_string(), "?"),
                ("Tab Focus:SERVICES".to_string(), "Tab"),
                ("r Restart".to_string(), "r"),
            ]
        );
        assert_eq!(
            wrapped.iter().flat_map(hint_texts).collect::<Vec<_>>(),
            vec![
                ("? Help".to_string(), "?"),
                ("Tab Focus:SERVICES".to_string(), "Tab"),
                ("r Restart".to_string(), "r"),
            ]
        );
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
    fn job_states_have_their_own_names() {
        let mut snapshot = micromux::ServiceSnapshot::initial(
            "seed".to_string(),
            "seed".to_string(),
            Vec::new(),
            None,
            micromux::RestartPolicy::Never,
            Vec::new(),
            None,
        );
        snapshot.kind = micromux::ServiceKind::Job;
        let mut named = |execution, exit_code, manual| {
            snapshot.execution = execution;
            snapshot.last_exit_code = exit_code;
            snapshot.manual = manual;
            (state_name(&snapshot), crate::style::is_frozen(&snapshot))
        };
        assert_eq!(
            named(micromux::Execution::Completed, Some(0), false),
            ("COMPLETED".to_string(), true)
        );
        assert_eq!(
            named(micromux::Execution::Failed, Some(137), false),
            ("FAILED 137".to_string(), false)
        );
        assert_eq!(
            named(micromux::Execution::Exited, Some(1), false),
            ("RETRYING".to_string(), false)
        );
        assert_eq!(
            named(micromux::Execution::Pending, None, true),
            ("IDLE".to_string(), false)
        );
    }

    #[test]
    fn command_rows_give_each_script_line_its_own_row() {
        let command_line = micromux::shell_join(["sh", "-c", "set -e\necho 'ok'"]);

        // The quoted script spans two rows, and each row keeps its quotes.
        assert_eq!(
            command_rows(&command_line),
            vec!["sh -c \"set -e".to_string(), "echo 'ok'\"".to_string()]
        );

        // A tab, which ratatui would drop, stays visible as its control picture.
        assert_eq!(
            command_rows(&micromux::shell_join(["printf", "a\tb"])),
            vec!["printf 'a␉b'".to_string()]
        );
    }

    #[test]
    fn border_detail_shortens_the_command_to_keep_the_facts_whole() {
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
        let mut view = LogView::default();
        let area = Rect::new(0, 0, 60, 4);
        let mut buf = Buffer::empty(area);
        let text = Text::default();
        let mut index = RenderedLineIndex::default();
        index.rebuild(&text, false, 56);

        view.render(
            area,
            &index,
            &text,
            PaneBorders {
                title: Line::raw("Logs"),
                status: None,
                detail: Some(service_detail(&snapshot, 1_000)),
                focused: false,
            },
            Highlights::default(),
            &mut buf,
        );

        // The generation ends at the bottom-right corner, and the command fills what is left up to
        // a gap of plain border.
        let bottom = row_text(&buf, 0, 3, 59);
        assert!(bottom.ends_with("xxx ... ────────── gen 1 ┘"), "{bottom}");
        assert!(bottom.starts_with("└ $ sh -c xxx"), "{bottom}");
    }

    #[test]
    fn border_detail_quotes_the_script_and_shows_its_line_breaks() {
        let snapshot = micromux::ServiceSnapshot::initial(
            "svc".to_string(),
            "svc".to_string(),
            Vec::new(),
            None,
            micromux::RestartPolicy::Never,
            vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo 'a'\necho b".to_string(),
            ],
            None,
        );

        // The script reads as one quoted argument, and its newline stays visible on the one-line
        // border as a picture rather than being dropped or turned into a `\n` escape that a
        // pasted line would pass to the script.
        let detail = service_detail(&snapshot, 1_000);
        assert_eq!(detail.left.content, " $ sh -c \"echo 'a'␊echo b\" ");
    }

    /// A retired dynamic service says why it retired and drops its dead lease, and a run whose
    /// config changed since it started says a restart applies it.
    #[test]
    fn service_info_reports_retirement_ownership_and_stale_config() {
        let mut snapshot = micromux::ServiceSnapshot::initial(
            "job".to_string(),
            "job".to_string(),
            Vec::new(),
            None,
            micromux::RestartPolicy::Never,
            Vec::new(),
            None,
        );
        snapshot.origin = micromux::OriginKind::Dynamic;
        snapshot.retired = Some(micromux::RetiredReason::Expired);
        snapshot.config_stale = true;
        snapshot.dynamic = Some(micromux::DynamicServiceInfo {
            revision: 2,
            expires_at_unix_ms: Some(5_000_000),
            owner: Some("agent-7".to_string()),
            created_at_unix_ms: 0,
        });

        let rows = super::service_info_rows(&snapshot, &|_| None, None, None, 1_000)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rows.contains("RETIRED  lease expired"), "{rows}");
        assert!(
            rows.contains("Config      changed since this run started"),
            "{rows}"
        );
        assert!(rows.contains("Dynamic     rev 2, owner agent-7"), "{rows}");
    }

    #[test]
    fn info_rows_wrap_under_their_value_column() {
        let row = Line::from(vec![
            ratatui::text::Span::raw("Command     "),
            ratatui::text::Span::raw("x".repeat(30)),
        ]);
        let rows = super::wrap_info_row(row, 24)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();
        // A wide character that would straddle the edge moves to the next row whole.
        let wide = Line::from(format!("{}{}", "x".repeat(23), "界界"));
        let wide_rows = super::wrap_info_row(wide, 24)
            .iter()
            .map(Line::width)
            .collect::<Vec<_>>();
        assert_eq!(wide_rows, vec![23, 12 + 4]);
        assert_eq!(
            rows,
            vec![
                format!("Command     {}", "x".repeat(12)),
                format!("{}{}", " ".repeat(12), "x".repeat(12)),
                format!("{}{}", " ".repeat(12), "x".repeat(6)),
            ]
        );
    }

    #[test]
    fn home_relative_paths_shorten_only_the_home_directory_itself() {
        use super::home_relative;
        use std::ffi::OsStr;

        let home = Some(OsStr::new("/home/ada"));
        assert_eq!(home_relative("/home/ada/work/app", home), "~/work/app");
        assert_eq!(home_relative("/home/ada", home), "~");
        assert_eq!(home_relative("/home/adam/app", home), "/home/adam/app");
        assert_eq!(home_relative("/srv/app", home), "/srv/app");
        assert_eq!(home_relative("/srv/app", None), "/srv/app");
    }

    #[test]
    fn truncation_ends_with_spaced_dots_within_the_limit() {
        use super::truncate_with_dots;

        // Text that fits stays as it is.
        assert_eq!(truncate_with_dots(" $ run --fast ", 20), " $ run --fast ");
        // A cut drops the whitespace before it and marks it with three dots set off by spaces.
        let cut = truncate_with_dots(" $ run --listen 127.0.0.1:8080 --verbose", 22);
        assert_eq!(cut, " $ run --listen 1 ... ");
        assert!(cut.chars().count() <= 22);
        assert_eq!(truncate_with_dots(" $ run --listen x", 12), " $ run ... ");
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
        let configured = service_detail(&snapshot, 1_000);
        assert_eq!(configured.left.content, " $ sh -c 'sleep 60' ");
        assert_eq!(configured.right.to_string(), " gen 3 ");

        // A running service also shows when its current run started, next to the generation.
        let started_at_unix_ms = 1_790_342_949_000;
        snapshot.started_at_unix_ms = Some(started_at_unix_ms);
        let started = short_local_time(started_at_unix_ms).unwrap_or_default();
        assert_eq!(
            service_detail(&snapshot, 1_000).right.to_string(),
            format!(" started {started} · gen 3 ")
        );
        snapshot.started_at_unix_ms = None;

        snapshot.origin = micromux::OriginKind::Dynamic;
        snapshot.dynamic = Some(micromux::DynamicServiceInfo {
            created_at_unix_ms: 0,
            expires_at_unix_ms: Some(61_000),
            owner: Some("agent".to_string()),
            revision: 2,
        });
        let dynamic = service_detail(&snapshot, 1_000).right.to_string();
        assert_eq!(
            dynamic,
            " dynamic · rev 2 · expires in ~1m · owner agent · gen 3 "
        );

        // A countdown on a dead lease would only mislead; retirement owns the status column.
        snapshot.retired = Some(micromux::RetiredReason::Expired);
        let retired = service_detail(&snapshot, 1_000).right.to_string();
        assert_eq!(retired, " dynamic · rev 2 · owner agent · gen 3 ");
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
        let (window, local_offset) =
            window_text(&text, &index, line_count - 1, 1, Highlights::default());
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
        let (window, _) = window_text(&text, &index, 0, 1, Highlights::default());
        let borrowed = window
            .lines
            .first()
            .and_then(|line| line.spans.first())
            .map(|span| span.content.as_ptr());

        assert_eq!(borrowed, original);
    }

    /// A match that crosses a span boundary and covers wide characters is highlighted cell for
    /// cell, over the colors of the spans it covers, and the current match apart from the rest.
    #[test]
    fn log_window_highlights_matches_across_spans_and_wide_characters() {
        use ratatui::style::{Color, Modifier, Style};
        use ratatui::widgets::Widget as _;

        let red = Style::new().fg(Color::Red);
        let text = Text::from(vec![
            Line::from(vec![Span::styled("日本", red), Span::raw("語 ok")]),
            Line::raw("ok ok"),
        ]);
        let mut index = RenderedLineIndex::default();
        index.rebuild(&text, false, 20);
        let matches = [
            // "本語" ends in the second span.
            crate::search::Match {
                line: 0,
                bytes: 3..9,
            },
            crate::search::Match {
                line: 1,
                bytes: 3..5,
            },
        ];
        let (window, _) = window_text(
            &text,
            &index,
            0,
            2,
            Highlights {
                matches: &matches,
                current: Some(1),
            },
        );
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        ratatui::widgets::Paragraph::new(window).render(area, &mut buf);

        let style_at = |x: u16, y: u16| {
            buf.cell((x, y))
                .map(|cell| (cell.symbol().to_string(), cell.fg, cell.modifier))
        };
        // "日" keeps its red; "本" and "語", two cells each, are reversed yellow; " ok" is plain.
        assert_eq!(
            style_at(0, 0),
            Some(("日".to_string(), Color::Red, Modifier::empty()))
        );
        assert_eq!(
            style_at(2, 0),
            Some(("本".to_string(), Color::Yellow, Modifier::REVERSED))
        );
        assert_eq!(
            style_at(4, 0),
            Some(("語".to_string(), Color::Yellow, Modifier::REVERSED))
        );
        assert_eq!(
            style_at(7, 0),
            Some(("o".to_string(), Color::Reset, Modifier::empty()))
        );
        // The current match is bold reverse video of the focus accent.
        assert_eq!(
            style_at(3, 1),
            Some((
                "o".to_string(),
                super::FOCUS_COLOR,
                Modifier::REVERSED | Modifier::BOLD
            ))
        );
        assert_eq!(
            style_at(0, 1),
            Some(("o".to_string(), Color::Reset, Modifier::empty()))
        );
        // Highlighting changes styles only, never the text.
        assert_eq!(
            window_text(&text, &index, 0, 1, Highlights::default())
                .0
                .lines
                .first()
                .map(Line::to_string),
            Some("日本語 ok".to_string())
        );
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

        let (window, local_offset) = window_text(&text, &index, 900, 10, Highlights::default());

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

/// Brings the healthcheck pane's text up to date with `source`, unless the attempts did not
/// change.
fn refresh_health_text(service: &mut crate::state::Service, source: &crate::SessionSource) {
    if !service.healthcheck_dirty {
        return;
    }
    let attempts = source.healthchecks(&service.snapshot.id);
    let out = build_healthcheck_text(service.snapshot.healthcheck_configured, &attempts);
    let text = out.as_str().into_text().unwrap_or_else(|err| {
        let escaped = strip_ansi_escapes::strip_str(&out);
        tracing::error!(
            ?err,
            input_bytes = out.len(),
            "failed to sanitize healthcheck output"
        );
        escaped.into()
    });
    service.health_pane.set_text(text);
    service.healthcheck_dirty = false;
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

/// Width of the label column in the info popups.
const INFO_LABEL_WIDTH: usize = 12;

/// One labelled row of an info popup; further values continue on rows of their own, aligned
/// under the first.
fn info_rows<V: Into<Line<'static>>>(label: &str, values: Vec<V>) -> Vec<Line<'static>> {
    let mut values = values.into_iter().map(Into::into);
    let first = values
        .next()
        .unwrap_or_else(|| Line::from("—".fg(Color::DarkGray)));
    let indent = |label: Span<'static>, line: Line<'static>| {
        let mut spans = vec![label];
        spans.extend(line.spans);
        Line::from(spans)
    };
    std::iter::once(indent(
        format!("{label:<INFO_LABEL_WIDTH$}")
            .fg(App::HEADER_COLOR)
            .bold(),
        first,
    ))
    .chain(values.map(|line| indent(" ".repeat(INFO_LABEL_WIDTH).into(), line)))
    .collect()
}

/// Splits an info popup row wider than `width` into rows of at most `width` columns, continuing
/// under the value column.
fn wrap_info_row(row: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let indent = INFO_LABEL_WIDTH.min(width / 2);
    if width == 0 || row.width() <= width {
        return vec![row];
    }
    let mut rows = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in row.spans {
        let mut text = String::new();
        for character in span.content.chars() {
            // Columns, not characters: a wide character takes two.
            let columns = Span::raw(character.to_string()).width();
            if used + columns > width && used > indent {
                if !text.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut text), span.style));
                }
                rows.push(Line::from(std::mem::take(&mut current)));
                current.push(" ".repeat(indent).into());
                used = indent;
            }
            text.push(character);
            used += columns;
        }
        if !text.is_empty() {
            current.push(Span::styled(text, span.style));
        }
    }
    if !current.is_empty() {
        rows.push(Line::from(current));
    }
    rows
}

/// Everything known about one service, for its details popup.
fn service_info_rows(
    snapshot: &micromux::ServiceSnapshot,
    dependency_kind: &dyn Fn(&str) -> Option<micromux::ServiceKind>,
    session: Option<&micromux::SessionSummary>,
    last_healthcheck: Option<&micromux::HealthAttempt>,
    now_unix_ms: u64,
) -> Vec<Line<'static>> {
    let mut state = vec![state_name(snapshot).set_style(crate::style::service_style(snapshot))];
    if let Some(reason) = snapshot.retired {
        let reason = match reason {
            micromux::RetiredReason::Stopped => "stopped",
            micromux::RetiredReason::Expired => "lease expired",
            micromux::RetiredReason::Removed => "removed from the config",
            micromux::RetiredReason::Unknown => "unknown reason",
        };
        state.push(format!("  {reason}").fg(Color::DarkGray));
    }
    if let Some(pid) = snapshot.pid {
        state.push(format!("  pid {pid}").fg(Color::DarkGray));
    }
    let urls = service_urls(session, &snapshot.id)
        .into_iter()
        .map(|url| url.fg(Color::LightBlue))
        .collect();
    let depends_on = snapshot
        .depends_on
        .iter()
        .map(|dependency| {
            let kind = dependency_kind(&dependency.service).unwrap_or_default();
            let condition = match dependency.condition_for(kind) {
                micromux::DependencyCondition::Started => "started",
                micromux::DependencyCondition::Healthy => "healthy",
                micromux::DependencyCondition::CompletedSuccessfully => "completed",
            };
            Span::raw(format!("{} ({condition})", dependency.service))
        })
        .collect();
    let stop = format!(
        "{}, then SIGKILL after {}s",
        snapshot.stop_signal,
        snapshot.stop_grace_period.as_secs()
    );
    let mut run = vec![format!("generation {}", snapshot.run_generation)];
    run.extend(
        snapshot
            .started_at_unix_ms
            .and_then(short_local_time)
            .map(|started| format!("started {started}")),
    );
    run.extend(
        snapshot
            .last_exit_code
            .map(|code| format!("last exit code {code}")),
    );

    let mut rows = info_rows("State", vec![Line::from(state)]);
    if snapshot.config_stale {
        rows.extend(info_rows(
            "Config",
            vec!["changed since this run started; restart to apply".fg(Color::Yellow)],
        ));
    }
    rows.extend(info_rows("URLs", urls));
    rows.extend(info_rows("Ports", port_rows(snapshot, session)));
    rows.extend(info_rows(
        "Command",
        command_rows(&micromux::shell_join(&snapshot.command)),
    ));
    rows.extend(info_rows(
        "Directory",
        snapshot
            .working_dir
            .iter()
            .map(|dir| home_relative(dir, std::env::var_os("HOME").as_deref()))
            .collect(),
    ));
    rows.extend(info_rows("Depends on", depends_on));
    if let Some(failure) = &snapshot.dependency_failure {
        rows.extend(info_rows(
            "Blocked",
            vec![failure.describe().fg(Color::Red)],
        ));
    }
    rows.extend(info_rows("Restart", restart_rows(snapshot)));
    rows.extend(info_rows("Stop", vec![stop]));
    rows.extend(info_rows(
        "Healthcheck",
        healthcheck_rows(snapshot, last_healthcheck),
    ));
    rows.extend(info_rows("Run", run));
    if let Some(cleanup) = &snapshot.last_cleanup {
        rows.extend(info_rows("Cleanup", vec![cleanup_phrase(cleanup)]));
    }
    if let Some(dynamic) = &snapshot.dynamic {
        let mut facts = vec![format!("rev {}", dynamic.revision)];
        // A retired service's lease no longer means anything.
        if snapshot.retired.is_none() {
            facts.push(lease_phrase(dynamic.expires_at_unix_ms, now_unix_ms));
        }
        if let Some(owner) = &dynamic.owner {
            facts.push(format!("owner {owner}"));
        }
        rows.extend(info_rows("Dynamic", vec![facts.join(", ")]));
    }
    rows
}

/// `path` with a leading `home` directory written as `~`, as a shell prompt shows it.
fn home_relative(path: &str, home: Option<&std::ffi::OsStr>) -> String {
    let rest = home
        .and_then(std::ffi::OsStr::to_str)
        .filter(|home| !home.is_empty() && *home != "/")
        .and_then(|home| path.strip_prefix(home.trim_end_matches('/')))
        .filter(|rest| rest.is_empty() || rest.starts_with('/'));
    rest.map_or_else(|| path.to_string(), |rest| format!("~{rest}"))
}

/// The service's advertised ports, each with the allocated variables that hold it.
fn port_rows(
    snapshot: &micromux::ServiceSnapshot,
    session: Option<&micromux::SessionSummary>,
) -> Vec<String> {
    let allocated = session.map(|session| &session.allocated_ports);
    snapshot
        .advertised_ports
        .iter()
        .map(|port| {
            let names = allocated
                .into_iter()
                .flatten()
                .filter(|(_, allocated)| *allocated == port)
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>();
            if names.is_empty() {
                format!(":{port}")
            } else {
                format!(":{port}  {}", names.join(", "))
            }
        })
        .collect()
}

/// The restart policy and any backoff in progress.
fn restart_rows(snapshot: &micromux::ServiceSnapshot) -> Vec<Span<'static>> {
    let policy = match &snapshot.restart_policy {
        micromux::RestartPolicy::Always => "always".to_string(),
        micromux::RestartPolicy::UnlessStopped => "unless stopped".to_string(),
        micromux::RestartPolicy::Never => "never".to_string(),
        micromux::RestartPolicy::OnFailure { max_attempts: None } => "on failure".to_string(),
        micromux::RestartPolicy::OnFailure {
            max_attempts: Some(max),
        } => format!("on failure, at most {max} times"),
    };
    let mut rows = vec![Span::raw(policy)];
    if let Some(state) = &snapshot.restart_state {
        let left = state
            .restarts_remaining
            .map(|remaining| format!(", {remaining} left"))
            .unwrap_or_default();
        rows.push(
            format!(
                "backing off {}s before the next attempt{left}",
                state.backoff_delay.as_secs()
            )
            .fg(Color::DarkGray),
        );
    }
    rows
}

/// How the latest healthcheck attempt went and what it ran.
fn healthcheck_rows(
    snapshot: &micromux::ServiceSnapshot,
    last: Option<&micromux::HealthAttempt>,
) -> Vec<Span<'static>> {
    match (&snapshot.healthcheck, last) {
        (None, _) => Vec::new(),
        (Some(_), None) => vec!["no attempt yet".fg(Color::DarkGray)],
        (Some(_), Some(attempt)) => {
            let outcome = match attempt.result {
                None => "running".to_string(),
                Some(result) if result.cancelled => "cancelled".to_string(),
                Some(result) if result.success => "ok".to_string(),
                Some(result) => format!("failed with exit code {}", result.exit_code),
            };
            let mut rows = vec![Span::raw(format!("last attempt {outcome}"))];
            rows.extend(
                command_rows(&format!("$ {}", attempt.command))
                    .into_iter()
                    .map(|row| row.fg(Color::DarkGray)),
            );
            rows
        }
    }
}

/// How the latest cleanup command ended.
fn cleanup_phrase(cleanup: &micromux::CleanupStatus) -> String {
    let outcome = if cleanup.timed_out {
        "timed out".to_string()
    } else if cleanup.success {
        "succeeded".to_string()
    } else {
        cleanup.exit_code.map_or_else(
            || "failed".to_string(),
            |code| format!("failed with exit code {code}"),
        )
    };
    format!("{outcome} (generation {})", cleanup.run_generation)
}

/// The session's name, proxy and every route, for the session popup.
fn session_info_rows(session: Option<&micromux::SessionSummary>) -> Vec<Line<'static>> {
    let Some(session) = session else {
        return vec![Line::raw("This session reports no session details.")];
    };
    let mut rows = info_rows("Session", vec![Span::raw(session.domain.clone())]);
    let routing = match &session.routing {
        None => String::new(),
        Some(micromux::RoutingStatus::Pending) => "  registering".to_string(),
        Some(micromux::RoutingStatus::Registered) => "  serving".to_string(),
        Some(micromux::RoutingStatus::Unavailable { reason }) => format!("  unavailable: {reason}"),
    };
    rows.extend(info_rows(
        "Proxy",
        vec![Span::raw(format!(":{}{routing}", session.proxy_port))],
    ));
    rows.push(Line::default());
    if session.routes.is_empty() {
        rows.extend(info_rows("Routes", Vec::<Line<'static>>::new()));
    } else {
        let width = session
            .routes
            .iter()
            .map(|route| route.display_url().chars().count())
            .max()
            .unwrap_or_default();
        rows.extend(info_rows(
            "Routes",
            session
                .routes
                .iter()
                .map(|route| {
                    let url = route.display_url();
                    let service = route.service.as_deref().unwrap_or("no single service");
                    let catch_all = if route.host == micromux::session::CATCH_ALL_HOST {
                        "  (catch-all)"
                    } else {
                        ""
                    };
                    let stripped = if route.strip_prefix {
                        "  (prefix stripped)"
                    } else {
                        ""
                    };
                    Span::raw(format!(
                        "{url:<width$}  → :{}  {service}{catch_all}{stripped}",
                        route.port
                    ))
                })
                .collect(),
        ));
    }
    rows.push(Line::default());
    rows.extend(info_rows(
        "Ports",
        session
            .allocated_ports
            .iter()
            .map(|(name, port)| Span::raw(format!(":{port}  {name}")))
            .collect(),
    ));
    rows
}

fn state_name(snapshot: &micromux::ServiceSnapshot) -> String {
    if snapshot.retired.is_some() {
        return "RETIRED".to_string();
    }
    if snapshot.desired == micromux::Desired::Disabled {
        return "DISABLED".to_string();
    }

    let job = snapshot.kind == micromux::ServiceKind::Job;
    let name = match snapshot.execution {
        micromux::Execution::Pending if snapshot.manual => "IDLE",
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
        // A job's failed run waits for its retry.
        micromux::Execution::Exited if job => "RETRYING",
        micromux::Execution::Exited => "EXITED",
        micromux::Execution::Completed => "COMPLETED",
        micromux::Execution::Failed => {
            return snapshot
                .last_exit_code
                .map_or_else(|| "FAILED".to_string(), |code| format!("FAILED {code}"));
        }
        micromux::Execution::Unknown => "UNKNOWN",
    };
    name.to_string()
}

/// Splits a [`micromux::shell_join`] command line into the rows an info popup shows.
///
/// A multi-line script keeps its newlines inside the shell quotes, so each line of the command
/// gets a row of its own; any other control character shows as [`control_pictures`] does.
fn command_rows(command_line: &str) -> Vec<String> {
    command_line.split('\n').map(control_pictures).collect()
}

/// Shows each control character of `text` as its Unicode control picture, such as `␊` for a
/// newline and `␉` for a tab.
///
/// The `ratatui` renderer drops control characters, which would silently change a displayed
/// command.
/// A picture keeps it visible without looking like shell syntax, so a reader cannot mistake it for
/// an escape the shell would interpret.
fn control_pictures(text: &str) -> String {
    text.chars()
        .map(|ch| match u32::from(ch) {
            code @ 0..0x20 => char::from_u32(0x2400 + code).unwrap_or(char::REPLACEMENT_CHARACTER),
            0x7f => '\u{2421}',
            _ if ch.is_control() => char::REPLACEMENT_CHARACTER,
            _ => ch,
        })
        .collect()
}

/// One key binding shown in the header or footer.
struct KeyHint {
    keys: &'static str,
    description: String,
    /// Current state of a toggle, emphasized after the description.
    value: Option<String>,
}

impl KeyHint {
    fn new(keys: &'static str, description: impl Into<String>) -> Self {
        Self {
            keys,
            description: description.into(),
            value: None,
        }
    }

    /// A hint for a toggle, rendered as `keys label:value`.
    fn toggle(keys: &'static str, label: &str, value: impl Into<String>) -> Self {
        Self {
            keys,
            description: format!("{label}:"),
            value: Some(value.into()),
        }
    }
}

/// Space between a popup's border and its content.
const POPUP_PADDING: ratatui::widgets::Padding = ratatui::widgets::Padding::symmetric(3, 1);

/// A popup centered in `area`, sized for `content_width` columns and `content_rows` rows of
/// content plus its border and padding, and capped at the area.
fn centered_popup(area: Rect, content_width: usize, content_rows: usize) -> Rect {
    let horizontal_chrome = POPUP_PADDING.left + POPUP_PADDING.right + 2;
    let vertical_chrome = POPUP_PADDING.top + POPUP_PADDING.bottom + 2;
    let width = rows_u16(content_width)
        .saturating_add(horizontal_chrome)
        .min(area.width);
    let height = rows_u16(content_rows)
        .saturating_add(vertical_chrome)
        .min(area.height);
    Rect {
        x: area.x.saturating_add(area.width.saturating_sub(width) / 2),
        y: area
            .y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    }
}

/// The keys of the one hint for both side panes, which take turns beside the logs and so share
/// the header's room.
pub(crate) const SIDE_PANE_KEYS: &str = "H/E";

/// Where each part of the screen goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScreenLayout {
    pub header: Rect,
    /// Everything between the header and the footer, where popups center.
    pub main: Rect,
    pub services: Rect,
    /// The logs pane, borders and scrollbar included.
    pub logs: Rect,
    /// The side pane, or an empty area while it is hidden.
    pub side: Rect,
    pub footer: Rect,
}

/// Heights of the header and footer rows around the panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChromeRows {
    pub header: u16,
    pub footer: u16,
}

pub(crate) fn rows_u16(rows: usize) -> u16 {
    u16::try_from(rows).unwrap_or(u16::MAX)
}

/// Each hint's first column, counted from its row's left edge, its width and its keys.
type HintPlaces = Vec<(usize, usize, &'static str)>;

/// One row of the header or footer, and where on it each key hint sits.
#[derive(Debug, Clone, Default)]
pub(crate) struct HintRow {
    pub(crate) line: Line<'static>,
    pub(crate) hints: HintPlaces,
}

impl HintRow {
    /// A row without hints.
    fn plain(line: Line<'static>) -> Self {
        Self {
            line,
            hints: Vec::new(),
        }
    }

    /// The row moved `columns` to the right, as when text is put before it.
    fn shifted(mut self, columns: usize) -> Self {
        for (x, _, _) in &mut self.hints {
            *x += columns;
        }
        self
    }

    /// The row right-aligned in `width` columns, as the header draws its wrapped controls.
    fn right_aligned(self, width: usize) -> Self {
        let shift = width.saturating_sub(self.line.width());
        let mut row = self.shifted(shift);
        row.line = row.line.right_aligned();
        row
    }
}

/// Packs key hints into rows no wider than `width`, never splitting a hint across rows.
///
/// A hint wider than `width` gets a row of its own and is truncated when drawn.
fn pack_key_hints(hints: &[KeyHint], width: usize) -> Vec<HintRow> {
    const FIRST_GAP: usize = 1;
    const GAP: usize = 2;

    let mut rows: Vec<(Vec<Span<'static>>, HintPlaces)> = Vec::new();
    let mut row_width = 0;
    for KeyHint {
        keys,
        description,
        value,
    } in hints
    {
        let mut hint = vec![
            keys.fg(Color::Yellow).bold(),
            format!(" {description}").fg(Color::DarkGray),
        ];
        if let Some(value) = value {
            hint.push(value.clone().fg(Color::DarkGray).bold());
        }
        let hint_width = hint.iter().map(Span::width).sum::<usize>();
        match rows.last_mut() {
            Some((row, at)) if row_width + GAP + hint_width <= width => {
                row.push(" ".repeat(GAP).into());
                row.extend(hint);
                at.push((row_width + GAP, hint_width, *keys));
                row_width += GAP + hint_width;
            }
            _ => {
                let mut row = vec![" ".repeat(FIRST_GAP).into()];
                row.extend(hint);
                rows.push((row, vec![(FIRST_GAP, hint_width, *keys)]));
                row_width = FIRST_GAP + hint_width;
            }
        }
    }
    rows.into_iter()
        .map(|(spans, hints)| HintRow {
            line: Line::from(spans),
            hints,
        })
        .collect()
}

/// A group of key bindings in the help overlay.
struct HelpSection {
    title: &'static str,
    keys: &'static [HelpKey],
}

struct HelpKey {
    keys: &'static str,
    description: &'static str,
}

/// Every key binding, grouped for the help overlay and laid out in two columns so it fits a
/// standard 80x24 terminal.
///
/// View keys change only what the panes show; the others move around the TUI or act on services
/// and the session.
const HELP_COLUMNS: [&[HelpSection]; 2] = [
    &[
        HelpSection {
            title: "View",
            keys: &[
                HelpKey {
                    keys: "L",
                    description: "Log level threshold",
                },
                HelpKey {
                    keys: "T",
                    description: "Timestamps on JSON lines",
                },
                HelpKey {
                    keys: "F",
                    description: "Fields hidden by config",
                },
                HelpKey {
                    keys: "w",
                    description: "Line wrapping",
                },
                HelpKey {
                    keys: "t",
                    description: "Follow tail",
                },
                HelpKey {
                    keys: "H",
                    description: "Healthcheck pane",
                },
                HelpKey {
                    keys: "A",
                    description: "All services' logs",
                },
            ],
        },
        HelpSection {
            title: "Navigate",
            keys: &[
                HelpKey {
                    keys: "↑/↓ j/k",
                    description: "Select or scroll",
                },
                HelpKey {
                    keys: "g / G",
                    description: "Top / bottom",
                },
                HelpKey {
                    keys: "←/→ h/l",
                    description: "Resize sidebar",
                },
                HelpKey {
                    keys: "Tab",
                    description: "Switch pane focus",
                },
                HelpKey {
                    keys: "m",
                    description: "Mouse capture; Shift bypasses",
                },
                HelpKey {
                    keys: "y / ↵",
                    description: "Copy selection; right-click too",
                },
            ],
        },
    ],
    &[
        HelpSection {
            title: "Inspect",
            keys: &[
                HelpKey {
                    keys: "i",
                    description: "Service details",
                },
                HelpKey {
                    keys: "u",
                    description: "Session URLs and ports",
                },
                HelpKey {
                    keys: "E",
                    description: "Lifecycle events pane",
                },
                HelpKey {
                    keys: "/",
                    description: "Search logs (Esc clears)",
                },
                HelpKey {
                    keys: "n / N",
                    description: "Older / newer match",
                },
            ],
        },
        HelpSection {
            title: "Act",
            keys: &[
                HelpKey {
                    keys: "r",
                    description: "Restart service",
                },
                HelpKey {
                    keys: "R",
                    description: "Restart all services",
                },
                HelpKey {
                    keys: "d",
                    description: "Disable / enable service",
                },
                HelpKey {
                    keys: "s",
                    description: "Stop dynamic service",
                },
                HelpKey {
                    keys: "a",
                    description: "PTY input (Ctrl+] exits)",
                },
                HelpKey {
                    keys: "Ctrl-Z",
                    description: "Suspend to the shell",
                },
                HelpKey {
                    keys: "q",
                    description: "Quit / detach",
                },
                HelpKey {
                    keys: "?",
                    description: "This help (Esc closes)",
                },
            ],
        },
    ],
];

fn help_column_rows(sections: &[HelpSection]) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for section in sections {
        if !rows.is_empty() {
            rows.push(Line::default());
        }
        rows.push(Line::from(section.title.bold().fg(App::HEADER_COLOR)));
        for HelpKey { keys, description } in section.keys {
            rows.push(Line::from(vec![
                format!("  {keys:<9}").fg(Color::Yellow).bold(),
                description.fg(Color::DarkGray),
            ]));
        }
    }
    rows
}

/// Places the help columns side by side, padding each row of a column to its widest row.
fn help_rows() -> Vec<Line<'static>> {
    const COLUMN_GAP: usize = 4;

    let columns = HELP_COLUMNS.map(help_column_rows);
    let height = columns.iter().map(Vec::len).max().unwrap_or_default();
    let widths = columns
        .each_ref()
        .map(|rows| rows.iter().map(Line::width).max().unwrap_or_default());
    (0..height)
        .map(|index| {
            let mut spans = Vec::new();
            for (rows, width) in columns.iter().zip(widths) {
                if !spans.is_empty() {
                    spans.push(" ".repeat(COLUMN_GAP).into());
                }
                let row = rows.get(index).cloned().unwrap_or_default();
                let padding = width.saturating_sub(row.width());
                spans.extend(row.spans);
                spans.push(" ".repeat(padding).into());
            }
            Line::from(spans)
        })
        .collect()
}

/// The logs pane's top-right corner: the filters of `format`, if any, and the search, or `None`
/// when nothing is hidden or searched for.
fn logs_status(
    format: Option<&crate::json_log::LineFormat>,
    search: Option<crate::search::SearchStatus<'_>>,
) -> Option<Line<'static>> {
    let mut filters = Vec::new();
    if let Some(min_level) = format.and_then(|format| format.min_level) {
        filters.push(format!(
            "level ≥ {}",
            crate::level::threshold_label(Some(min_level))
        ));
    }
    match format.map_or(0, |format| format.hidden_fields.len()) {
        0 => {}
        1 => filters.push("1 field hidden".to_string()),
        hidden => filters.push(format!("{hidden} fields hidden")),
    }
    let mut facts = filters
        .into_iter()
        .map(|filter| filter.fg(Color::DarkGray))
        .collect::<Vec<_>>();
    if let Some(search) = search {
        facts.push(search_fact(search));
    }
    (!facts.is_empty()).then(|| border_facts(facts))
}

/// Longest query the logs pane's border shows in full.
const MAX_SHOWN_QUERY_CHARS: usize = 24;

/// The search for the logs pane's border: the query and which match is current, how many there
/// are, or that there is none.
fn search_fact(search: crate::search::SearchStatus<'_>) -> Span<'static> {
    let crate::search::SearchStatus {
        query,
        current,
        count,
    } = search;
    let query = if query.chars().count() > MAX_SHOWN_QUERY_CHARS {
        let kept = query
            .chars()
            .take(MAX_SHOWN_QUERY_CHARS - 1)
            .collect::<String>();
        format!("{kept}…")
    } else {
        query.to_string()
    };
    match (current, count) {
        (_, 0) => format!("/{query} no match").fg(Color::Red),
        (Some(index), count) => format!("/{query} {}/{count}", index + 1).fg(Color::Yellow),
        (None, 1) => format!("/{query} 1 match").fg(Color::Yellow),
        (None, count) => format!("/{query} {count} matches").fg(Color::Yellow),
    }
}

/// Fewest border dashes kept between the Logs title and the filters on the right.
const LOGS_TITLE_GAP: usize = 2;

/// The Logs pane title: "Logs", then the browser URLs of the routes the service serves, as many
/// as fit in `width` columns, with the rest counted.
///
/// Also returns each URL shown with the column it starts at in the title.
fn logs_title(urls: &[String], width: usize) -> (Line<'static>, Vec<(usize, String)>) {
    let separator = || " · ".fg(Color::DarkGray);
    let more = |hidden: usize| format!("+{hidden} more").fg(Color::DarkGray);
    let mut spans = vec![Span::raw("Logs")];
    // A trailing space keeps the border dashes out of the last URL, which terminals that detect
    // links would otherwise take as part of it.
    let mut used = "Logs".len() + 1;
    let mut columns = Vec::new();
    for (shown, url) in urls.iter().enumerate() {
        let hidden_after = urls.len() - shown - 1;
        // A URL fits when the count of the ones after it still fits behind it.
        let count_width = if hidden_after == 0 {
            0
        } else {
            3 + more(hidden_after).width()
        };
        let url_width = 3 + url.chars().count();
        if used + url_width + count_width > width {
            let hidden = urls.len() - shown;
            if used + 3 + more(hidden).width() <= width {
                spans.extend([separator(), more(hidden)]);
            }
            break;
        }
        // The URL follows "Logs" and the separators so far, each three columns wide.
        let start = spans.iter().map(Span::width).sum::<usize>() + 3;
        spans.extend([separator(), url.clone().fg(Color::LightBlue)]);
        columns.push((start, url.clone()));
        used += url_width;
    }
    if spans.len() > 1 {
        spans.push(Span::raw(" "));
    }
    (Line::from(spans), columns)
}

/// The browser URLs of the routes whose port belongs to `service`, in config order.
fn service_urls(session: Option<&micromux::SessionSummary>, service: &str) -> Vec<String> {
    session
        .map(|session| {
            session
                .routes
                .iter()
                .filter(|route| route.service.as_deref() == Some(service))
                .map(|route| route.display_url().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Joins facts drawn into a border with white dots, padded by a space on either end.
fn border_facts(facts: impl IntoIterator<Item = Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    spans.extend(intersperse(facts, " · ".fg(Color::DarkGray)));
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn level_color(min_level: Option<micromux::StructuredLogLevel>) -> Color {
    match min_level {
        None => Color::Reset,
        Some(micromux::StructuredLogLevel::Trace) => Color::DarkGray,
        Some(micromux::StructuredLogLevel::Debug) => Color::Cyan,
        Some(micromux::StructuredLogLevel::Info) => Color::Green,
        Some(micromux::StructuredLogLevel::Warn) => Color::Yellow,
        Some(micromux::StructuredLogLevel::Error) => Color::Red,
        Some(micromux::StructuredLogLevel::Fatal) => Color::Magenta,
    }
}

/// Shortens `text` to at most `max_chars`, ending a cut with a spaced `...` so it reads apart
/// from the border line that follows.
fn truncate_with_dots(text: &str, max_chars: usize) -> String {
    const MARKER: &str = " ... ";

    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let kept = max_chars.saturating_sub(MARKER.len());
    let mut truncated = text.chars().take(kept).collect::<String>();
    truncated.truncate(truncated.trim_end().len());
    truncated.push_str(MARKER);
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

/// A pane's frame, which shows whether the pane has the focus: thick and in the accent color
/// when it has, plain and dim when not.
///
/// The thickness alone tells the focused pane apart where the terminal shows no color.
fn pane_block<'a>(focused: bool) -> Block<'a> {
    let block = Block::default().borders(Borders::ALL);
    if focused {
        block
            .border_type(ratatui::widgets::BorderType::Thick)
            .border_style(Style::default().fg(FOCUS_COLOR))
    } else {
        block.border_style(Style::default().fg(UNFOCUSED_COLOR))
    }
}

/// A pane's name for its frame's title: bold and in the accent color when the pane has the
/// focus, dim when not.
fn pane_name(name: &str, focused: bool) -> Span<'static> {
    let span = Span::raw(name.to_string());
    if focused {
        span.bold().fg(FOCUS_COLOR)
    } else {
        span.fg(Color::DarkGray)
    }
}

/// The accent of the focused pane's frame.
pub(crate) const FOCUS_COLOR: Color = Color::Cyan;

/// The frame of a pane without the focus.
const UNFOCUSED_COLOR: Color = Color::DarkGray;

/// Color of the command in the logs pane's bottom border.
const BORDER_COMMAND_COLOR: Color = Color::DarkGray;
/// Color of the run facts in the logs pane's bottom border.
const BORDER_RUN_FACTS_COLOR: Color = Color::Magenta;

/// Identity of the selected service for the logs pane frame.
///
/// The command it runs goes on the left, where the border shortens it to fit.
/// The bounded facts go on the right in full: for dynamic services the definition revision plus
/// the lease and ownership facts, then when the current run started and its generation.
fn service_detail(
    snapshot: &micromux::ServiceSnapshot,
    now_unix_ms: u64,
) -> log_view::BorderDetail {
    // The border is a single line, so even the newlines of a multi-line script show as pictures.
    let command = control_pictures(&micromux::shell_join(&snapshot.command));
    let left = if command.is_empty() {
        Span::default()
    } else {
        format!(" $ {command} ").fg(BORDER_COMMAND_COLOR)
    };

    let mut facts: Vec<Span<'static>> = Vec::new();
    if snapshot.origin == micromux::OriginKind::Dynamic {
        let mut dynamic_facts = vec!["dynamic".to_string()];
        if let Some(dynamic) = &snapshot.dynamic {
            dynamic_facts.push(format!("rev {}", dynamic.revision));
            // Retirement already owns the status column; a countdown on a dead lease would only
            // mislead.
            if snapshot.retired.is_none() {
                dynamic_facts.push(lease_phrase(dynamic.expires_at_unix_ms, now_unix_ms));
            }
            if let Some(owner) = &dynamic.owner {
                dynamic_facts.push(format!("owner {owner}"));
            }
        }
        facts.extend(dynamic_facts.into_iter().map(|fact| fact.fg(Color::Yellow)));
    }
    if let Some(started) = snapshot.started_at_unix_ms.and_then(short_local_time) {
        facts.push(format!("started {started}").fg(BORDER_RUN_FACTS_COLOR));
    }
    facts.push(format!("gen {}", snapshot.run_generation).fg(BORDER_RUN_FACTS_COLOR));
    log_view::BorderDetail {
        left,
        right: border_facts(facts),
    }
}

/// Formats a Unix millisecond timestamp as a short local date and time, such as `Sep 25 13:29:09`.
fn short_local_time(unix_ms: u64) -> Option<String> {
    let timestamp = chrono::DateTime::from_timestamp_millis(i64::try_from(unix_ms).ok()?)?;
    Some(
        timestamp
            .with_timezone(&chrono::Local)
            .format("%b %-d %H:%M:%S")
            .to_string(),
    )
}

impl Widget for &mut App {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let header = self.header_rows(area.width);
        let footer = self.footer_rows(area.width);
        let chrome = ChromeRows {
            header: rows_u16(header.len()),
            footer: rows_u16(footer.len()),
        };
        let layout = self.screen_layout(area, chrome);

        Paragraph::new(
            header
                .iter()
                .map(|row| row.line.clone())
                .collect::<Vec<_>>(),
        )
        .render(layout.header, buf);
        let (service_rows, first_service) = self.render_services(layout.services, buf);
        let links = self.render_logs(layout.logs, buf);
        if let Some(pane) = self.side_pane {
            self.render_side_pane(pane, layout.side, buf);
        }
        Paragraph::new(
            footer
                .iter()
                .map(|row| row.line.clone())
                .collect::<Vec<_>>(),
        )
        .render(layout.footer, buf);
        let covered = self.render_mouse_notice(&footer, layout.footer, buf);
        let covered = self.render_search_prompt(layout.footer, buf).or(covered);

        // The popups below record themselves as they draw.
        self.hits = crate::mouse::Hits {
            services: layout.services,
            service_rows,
            first_service,
            logs: layout.logs,
            side: layout.side,
            // The hints the quit hint covers cannot be clicked, since they do not show.
            hints: [(&header, layout.header), (&footer, layout.footer)]
                .into_iter()
                .flat_map(|(rows, area)| hint_hits(rows, area))
                .filter(|hit| Some(hit.area.y) != covered)
                .collect(),
            links,
            popup: None,
        };
        self.render_level_picker(layout.logs, buf);
        if let Some(view) = self.info {
            self.render_info(view, layout.main, buf);
        }
        if self.show_help {
            self.render_help(layout.main, buf);
        }
        self.paint_selection(buf);
    }
}

/// Where each key hint of `rows`, drawn in `area`, landed on screen.
fn hint_hits(rows: &[HintRow], area: Rect) -> Vec<crate::mouse::HintHit> {
    let mut hits = Vec::new();
    for (row, line) in rows.iter().zip(area.rows()) {
        for &(x, width, keys) in &row.hints {
            let x = area.x.saturating_add(rows_u16(x));
            if x < area.right() {
                hits.push(crate::mouse::HintHit {
                    area: Rect {
                        x,
                        y: line.y,
                        width: rows_u16(width).min(area.right() - x),
                        height: 1,
                    },
                    keys,
                });
            }
        }
    }
    hits
}

impl App {
    const HEADER_COLOR: Color = Color::Yellow;
    /// A selected row is bold and set apart by its marker rather than a fill, because no palette
    /// color makes a fill that reads well in both light and dark themes.
    const HIGHLIGHT_STYLE: Style = Style::new().add_modifier(Modifier::BOLD);

    fn attachment_header(&self) -> Option<Line<'static>> {
        let status = self.source.attachment_status()?;
        let mut spans = if status.connected {
            vec!["attached: ".fg(Color::Green).bold()]
        } else {
            vec!["reconnecting… ".fg(Color::Red).bold()]
        };
        spans.extend([
            status.session.name.bold(),
            format!(" ({})", status.session.config_path).into(),
        ]);
        if let Some(notice) = status.notice.as_deref().or_else(|| self.terminal_notice()) {
            spans.extend([" — ".into(), notice.to_string().fg(Color::Red)]);
        }
        Some(Line::from(spans))
    }

    fn local_warning_header(&self) -> Option<Line<'static>> {
        let notice = self
            .source
            .local_notice()
            .or_else(|| self.terminal_notice())?;
        Some(Line::from(vec![
            format!("micromux v{}", env!("CARGO_PKG_VERSION"))
                .bold()
                .fg(App::HEADER_COLOR),
            " — WARNING: ".fg(Color::Red).bold(),
            notice.to_string().fg(Color::Red),
        ]))
    }

    fn status_line(&self) -> Line<'static> {
        let mut status = self
            .attachment_header()
            .or_else(|| self.local_warning_header())
            .unwrap_or_else(|| {
                Line::from(
                    format!("micromux v{}", env!("CARGO_PKG_VERSION"))
                        .bold()
                        .fg(App::HEADER_COLOR),
                )
            });
        // Indent one column to line up with the pane titles drawn inside their borders.
        status.spans.insert(0, " ".into());
        status
    }

    /// The header rows at `width`: the session status with the view controls beside it, then why
    /// the proxy does not serve the session's URLs, if it does not.
    ///
    /// When the controls do not fit beside the status, they wrap onto right-aligned rows below
    /// it, so warnings and attach state always keep the first row.
    pub(crate) fn header_rows(&self, width: u16) -> Vec<HintRow> {
        let width = usize::from(width);
        let urls = self.routing_problem_line().map(HintRow::plain);
        let status = self.status_line();
        let controls = pack_key_hints(&self.view_key_hints(), width);
        if let [single_row] = controls.as_slice()
            && status.width() + 1 + single_row.line.width() <= width
        {
            let gap = width - status.width() - single_row.line.width();
            let shift = status.width() + gap;
            let row = single_row.clone().shifted(shift);
            let mut spans = status.spans;
            spans.push(" ".repeat(gap).into());
            spans.extend(row.line.spans);
            let first = HintRow {
                line: Line::from(spans),
                hints: row.hints,
            };
            return std::iter::once(first).chain(urls).collect();
        }
        std::iter::once(HintRow::plain(status))
            .chain(urls)
            .chain(controls.into_iter().map(|row| row.right_aligned(width)))
            .collect()
    }

    /// Why the proxy does not serve the session's URLs right now, if it does not.
    ///
    /// The URLs themselves sit in the Logs pane title of the service they belong to.
    fn routing_problem_line(&self) -> Option<Line<'static>> {
        let session = self.source.session_summary()?;
        if session.routes.is_empty() {
            return None;
        }
        let Some(micromux::RoutingStatus::Unavailable { reason }) = session.routing else {
            return None;
        };
        Some(Line::from(
            format!("routing unavailable: {reason}").fg(Color::Red),
        ))
    }

    /// The footer rows at `width`, wrapping whole key hints onto further rows as needed.
    pub(crate) fn footer_rows(&self, width: u16) -> Vec<HintRow> {
        pack_key_hints(&self.action_key_hints(), usize::from(width))
    }

    /// Header and footer heights at `width`; the panes get the rows in between.
    pub(crate) fn chrome_rows(&self, width: u16) -> ChromeRows {
        ChromeRows {
            header: rows_u16(self.header_rows(width).len()),
            footer: rows_u16(self.footer_rows(width).len()),
        }
    }

    /// Where the frame puts each part of the screen in `area`, with `chrome` rows for the header
    /// and footer.
    ///
    /// Drawing, hit-testing and the service PTY size all derive from it, so they cannot drift
    /// apart.
    pub(crate) fn screen_layout(&self, area: Rect, chrome: ChromeRows) -> ScreenLayout {
        let [header, main, footer] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(chrome.header),
                Constraint::Min(0),
                Constraint::Length(chrome.footer),
            ])
            .spacing(0)
            .areas(area);
        let [services, right] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(self.state.services_sidebar_width),
                Constraint::Min(0),
            ])
            .spacing(0)
            .areas(main);
        let [logs, side] = if self.side_pane.is_some() {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                .spacing(0)
                .areas(right)
        } else {
            [right, Rect::default()]
        };
        ScreenLayout {
            header,
            main,
            services,
            logs,
            side,
            footer,
        }
    }

    /// Keys that change only what the log panes show, each with its current state.
    fn view_key_hints(&self) -> Vec<KeyHint> {
        let on_off = |enabled: bool| if enabled { "ON" } else { "OFF" };
        let current = self.state.current_service();
        let level = current
            .map(|service| crate::level::threshold_label(service.min_level()))
            .unwrap_or_default();
        let timestamps = current.is_some_and(crate::state::Service::shows_timestamps);
        let fields = if current.is_some_and(crate::state::Service::filters_fields) {
            "FILTERED"
        } else {
            "ALL"
        };
        vec![
            KeyHint::toggle("L", "Level", level),
            KeyHint::toggle("T", "Time", on_off(timestamps)),
            KeyHint::toggle("F", "Fields", fields),
            KeyHint::toggle("w", "Wrap", on_off(self.log_view.wrap)),
            KeyHint::toggle("t", "Tail", on_off(self.log_view.follow_tail)),
            KeyHint::toggle(
                SIDE_PANE_KEYS,
                "Pane",
                match self.side_pane {
                    None => "OFF",
                    Some(crate::SidePane::Health) => "HEALTH",
                    Some(crate::SidePane::Events) => "EVENTS",
                },
            ),
            KeyHint::toggle("A", "All", on_off(self.state.all_logs.is_some())),
        ]
    }

    /// Keys that move around the TUI or act on services and the session.
    fn action_key_hints(&self) -> Vec<KeyHint> {
        let focus = match self.focus {
            crate::Focus::Services => "SERVICES",
            crate::Focus::Logs => "LOGS",
            crate::Focus::Side => match self.side_pane {
                Some(crate::SidePane::Events) => "EVENTS",
                Some(crate::SidePane::Health) | None => "HEALTH",
            },
        };
        let selected = self
            .state
            .current_service()
            .map(|service| &service.snapshot);
        let mut hints = vec![
            KeyHint::new("?", "Help"),
            KeyHint::new("i", "Info"),
            KeyHint::new("u", "URLs"),
            KeyHint::new("/", "Search"),
            KeyHint::new("↑/↓", "Move"),
            KeyHint::new("←/→", "Resize"),
            KeyHint::toggle("Tab", "Focus", focus),
            KeyHint::new("r", "Restart"),
            KeyHint::new("R", "Restart all"),
            KeyHint::new(
                "d",
                if selected.is_some_and(|snapshot| snapshot.desired == micromux::Desired::Disabled)
                {
                    "Enable"
                } else {
                    "Disable"
                },
            ),
        ];
        // Stopping only applies to a live dynamic service, so the hint appears only then.
        if selected.is_some_and(|snapshot| {
            snapshot.origin == micromux::OriginKind::Dynamic && snapshot.retired.is_none()
        }) {
            hints.push(KeyHint::new("s", "Stop"));
        }
        if self.input.is_some() {
            if self.pty_input_mode {
                hints.push(KeyHint::new("Ctrl+]", "Exit PTY input"));
            } else {
                hints.push(KeyHint::new("a", "PTY input"));
            }
        }
        // While text is selected, the copy key takes the mouse toggle's place, padded to its
        // width, so the footer keeps its rows and the panes and service terminals their size.
        // Capture is on whenever there is a selection, so the toggle has nothing to report.
        // The level picker takes `y` for nothing, so the hint would promise what it cannot do.
        if self.has_selection() && !self.pty_input_mode && self.level_picker.is_none() {
            hints.push(KeyHint::new("y", "Copy    "));
        } else {
            hints.push(KeyHint::toggle(
                "m",
                "Mouse",
                if self.mouse.capture { "ON" } else { "OFF" },
            ));
        }
        hints.push(KeyHint::new(
            "q",
            if self.source.attachment_status().is_some() {
                "Detach"
            } else {
                "Quit"
            },
        ));
        hints
    }

    fn terminal_notice(&self) -> Option<&str> {
        self.terminal_input_closed
            .then_some(crate::TERMINAL_INPUT_CLOSED_NOTICE)
            .or(self.input_notice.as_deref())
    }

    /// Draws the services list and returns the rows it was drawn in and the index of the service
    /// on the first of them.
    fn render_services(&mut self, area: Rect, buf: &mut Buffer) -> (Rect, usize) {
        let items: Vec<ListItem> = self
            .state
            .services
            .iter()
            .enumerate()
            .map(|(index, service)| {
                let status = format!("{: >10}", state_name(&service.snapshot))
                    .set_style(crate::style::service_style(&service.snapshot));

                let origin = match service.snapshot.origin {
                    micromux::OriginKind::Dynamic => "+",
                    micromux::OriginKind::Configured | micromux::OriginKind::Unknown => " ",
                };
                // Ports and URLs live in the `i` popup and the Logs title, not in the list.
                let line = Line::from(vec![
                    status,
                    " ".into(),
                    origin.fg(Color::DarkGray),
                    if index == self.state.selected_service {
                        service.snapshot.id.as_str().fg(FOCUS_COLOR).bold()
                    } else {
                        service.snapshot.id.as_str().into()
                    },
                ]);

                ListItem::new(line)
            })
            .collect();

        let block = pane_block(self.focus == crate::Focus::Services)
            .title(pane_name("Services", self.focus == crate::Focus::Services));
        let rows = block.inner(area);
        // The list scrolls on from where the last frame left it, so the rows a click lands on
        // stay put once the selection moves to one of them.
        // Rows freed by a taller pane or a shorter roster go to the services above, which ratatui
        // would leave scrolled out of view.
        let max_offset = self
            .state
            .services
            .len()
            .saturating_sub(usize::from(rows.height));
        let mut state = ListState::default()
            .with_offset(self.first_service.min(max_offset))
            .with_selected(Some(self.state.selected_service));
        let sidebar = List::new(items)
            .block(block)
            .highlight_style(Self::HIGHLIGHT_STYLE)
            // The marker takes the selected service's status color.
            .highlight_symbol(selection_marker(
                self.state
                    .current_service()
                    .map_or_else(Style::default, |service| {
                        crate::style::service_style(&service.snapshot)
                    }),
            ));
        StatefulWidget::render(&sidebar, area, buf, &mut state);
        self.first_service = state.offset();
        (rows, state.offset())
    }

    /// Reads new log records and renders the text the log pane shows for a pane `area` wide.
    ///
    /// A view off the tail stays on the rows it shows while new lines arrive and the oldest are
    /// dropped, which would otherwise shift the text up under it.
    fn update_log_text(&mut self, area: Rect) {
        let shown = self.state.shown_log();
        let top = shown
            .filter(|_| !self.log_view.follow_tail)
            .and_then(|shown| shown.log.row_anchor(self.log_view.scroll_offset));
        let generation = shown.map(|shown| shown.log.generation);
        self.state.update_shown_log(
            &self.source,
            self.pretty_json_logs,
            self.log_view.wrap,
            area.width.saturating_sub(3),
        );
        if let Some(top) = top
            && let Some(shown) = self.state.shown_log()
            && Some(shown.log.generation) != generation
        {
            self.log_view.scroll_offset = shown.log.anchored_row(top);
        }
    }

    /// Draws the logs the log pane shows and returns where the URLs in its title landed.
    fn render_logs(&mut self, area: Rect, buf: &mut Buffer) -> Vec<crate::mouse::LinkHit> {
        let session = self.source.session_summary();
        self.update_log_text(area);
        let Some(shown) = self.state.shown_log() else {
            return Vec::new();
        };
        self.search.refresh(shown);
        let focused = self.focus == crate::Focus::Logs;
        let Some(current_service) = shown.service else {
            // Every service's logs, whose own filters each service's pane names.
            self.log_view.render(
                area,
                &shown.log.line_index,
                &shown.log.text,
                log_view::PaneBorders {
                    title: Line::from(pane_name("All logs", focused)),
                    status: logs_status(None, self.search.status()),
                    detail: None,
                    focused,
                },
                self.search.highlights(),
                buf,
            );
            return Vec::new();
        };
        let format = current_service.line_format(self.pretty_json_logs);
        let detail = service_detail(&current_service.snapshot, now_unix_ms());
        let status = logs_status(Some(&format), self.search.status());
        // The pane's border spans its width less the scrollbar column and both corners.
        let title_width = usize::from(area.width)
            .saturating_sub(3)
            .saturating_sub(status.as_ref().map_or(0, Line::width))
            .saturating_sub(LOGS_TITLE_GAP);
        let (title, url_columns) = logs_title(
            &service_urls(session.as_ref(), &current_service.snapshot.id),
            title_width,
        );
        let mut title = title;
        if let Some(name) = title.spans.first_mut() {
            *name = pane_name("Logs", focused);
        }
        // The title starts right after the pane's top-left corner.
        let links = url_columns
            .into_iter()
            .map(|(column, url)| crate::mouse::LinkHit {
                area: Rect {
                    x: area.x.saturating_add(1).saturating_add(rows_u16(column)),
                    y: area.y,
                    width: rows_u16(url.chars().count()),
                    height: 1,
                },
                url,
            })
            .collect();
        tracing::trace!(
            service_id = current_service.snapshot.id,
            num_lines = shown.log.line_index.total_lines(),
            "collected logs"
        );

        self.log_view.render(
            area,
            &shown.log.line_index,
            &shown.log.text,
            log_view::PaneBorders {
                title,
                status,
                detail: Some(detail),
                focused,
            },
            self.search.highlights(),
            buf,
        );
        // A stopped service's logs are a record, not a live view, so the whole pane goes gray
        // without losing the shapes its colors drew.
        if crate::style::is_frozen(&current_service.snapshot) {
            crate::style::freeze(buf, area);
        }
        links
    }

    /// Draws `pane` for the selected service, reading what changed in the session since the
    /// last frame.
    fn render_side_pane(&mut self, pane: crate::SidePane, area: Rect, buf: &mut Buffer) {
        let focused = self.focus == crate::Focus::Side;
        let Some(service) = self.state.services.get_mut(self.state.selected_service) else {
            return;
        };
        let (title, text) = match pane {
            crate::SidePane::Health => {
                refresh_health_text(service, &self.source);
                ("Healthcheck", &mut service.health_pane)
            }
            crate::SidePane::Events => {
                service.refresh_events(&self.source);
                ("Events", &mut service.events_pane)
            }
        };
        text.lay_out(self.side_view.wrap, area.width.saturating_sub(3));
        self.side_view.render(
            area,
            &text.line_index,
            &text.text,
            log_view::PaneBorders {
                title: Line::from(pane_name(title, focused)),
                status: None,
                detail: None,
                focused,
            },
            log_view::Highlights::default(),
            buf,
        );
    }

    fn render_level_picker(&mut self, area: Rect, buf: &mut Buffer) {
        let Some(picker) = &self.level_picker else {
            return;
        };
        let current = self
            .state
            .services
            .iter()
            .find(|service| service.snapshot.id == picker.service_id)
            .map(crate::state::Service::min_level);
        let items = crate::level::LEVEL_CHOICES
            .iter()
            .map(|choice| {
                let label = crate::level::threshold_label(*choice);
                let mut spans = vec![format!("{label:<6}").fg(level_color(*choice))];
                if current == Some(*choice) {
                    spans.push(" (current)".fg(Color::DarkGray));
                }
                ListItem::new(Line::from(spans))
            })
            .collect::<Vec<_>>();

        let title = format!(" Level · {} ", picker.service_id);
        let hint = " ↵ select · Esc cancel ";
        let widest_item = items.iter().map(ListItem::width).max().unwrap_or_default();
        let content_width = title
            .chars()
            .count()
            .max(hint.chars().count())
            // The highlight symbol shares the row with each item.
            .max(widest_item.saturating_add(3));
        let popup = centered_popup(area, content_width, items.len());

        let block = Block::default()
            .borders(Borders::ALL)
            .padding(POPUP_PADDING)
            .title(title)
            .title_bottom(hint.fg(Color::DarkGray));
        let rows = block.inner(popup);
        let list = List::new(items)
            .block(block)
            .highlight_style(Self::HIGHLIGHT_STYLE)
            .highlight_symbol(selection_marker(Style::default().fg(FOCUS_COLOR)));
        let mut state = ListState::default();
        state.select(Some(picker.cursor));
        Clear.render(popup, buf);
        StatefulWidget::render(&list, popup, buf, &mut state);
        self.hits.popup = Some(crate::mouse::PopupHit {
            area: popup,
            content: crate::mouse::PopupContent::Levels {
                rows,
                first: state.offset(),
            },
        });
    }

    fn render_info(&mut self, view: crate::InfoView, area: Rect, buf: &mut Buffer) {
        let session = self.source.session_summary();
        let (title, rows) = match view {
            crate::InfoView::Service => {
                // The service can vanish under the popup, as when an attached session restarts;
                // an invisible popup must not keep swallowing keys.
                let Some(service) = self.state.current_service() else {
                    self.info = None;
                    return;
                };
                let attempts = self.source.healthchecks(&service.snapshot.id);
                (
                    format!(" Service · {} ", service.snapshot.id),
                    service_info_rows(
                        &service.snapshot,
                        &|id| {
                            self.state
                                .services
                                .iter()
                                .find(|service| service.snapshot.id == id)
                                .map(|service| service.snapshot.kind)
                        },
                        session.as_ref(),
                        attempts.last(),
                        now_unix_ms(),
                    ),
                )
            }
            crate::InfoView::Session => {
                let name = session
                    .as_ref()
                    .map_or_else(|| "no session".to_string(), |session| session.name.clone());
                (
                    format!(" Session · {name} "),
                    session_info_rows(session.as_ref()),
                )
            }
        };
        let key = match view {
            crate::InfoView::Service => "i",
            crate::InfoView::Session => "u",
        };
        // Rows wider than the popup can grow wrap under their value column, and rows beyond its
        // height scroll, so nothing is cut off silently.
        let horizontal_chrome = usize::from(POPUP_PADDING.left + POPUP_PADDING.right + 2);
        let vertical_chrome = usize::from(POPUP_PADDING.top + POPUP_PADDING.bottom + 2);
        let wrap_width = usize::from(area.width).saturating_sub(horizontal_chrome);
        let rows = rows
            .into_iter()
            .flat_map(|row| wrap_info_row(row, wrap_width))
            .collect::<Vec<_>>();
        let content_width = rows.iter().map(Line::width).max().unwrap_or_default();
        let popup = centered_popup(area, content_width, rows.len());
        let visible = usize::from(popup.height).saturating_sub(vertical_chrome);
        let max_scroll = rows.len().saturating_sub(visible);
        self.popup_scroll = self.popup_scroll.min(max_scroll);
        let mut block = Block::default()
            .borders(Borders::ALL)
            .padding(POPUP_PADDING)
            .title(title)
            .title_bottom(format!(" {key} or Esc closes ").fg(Color::DarkGray));
        if max_scroll > 0 {
            block = block.title_bottom(self.scroll_position(visible, rows.len()));
        }
        self.render_text_popup(crate::mouse::TextPane::Info(view), popup, block, rows, buf);
    }

    /// Where a scrolled popup is, for its bottom border's right side.
    fn scroll_position(&self, visible: usize, rows: usize) -> Line<'static> {
        let first = self.popup_scroll + 1;
        let last = (self.popup_scroll + visible).min(rows);
        format!(" ↑/↓ scroll · {first}-{last} of {rows} ")
            .fg(Color::DarkGray)
            .into_right_aligned_line()
    }

    /// Draws a popup of `rows` scrolled to the current popup row, and records it for the mouse.
    fn render_text_popup(
        &mut self,
        pane: crate::mouse::TextPane,
        popup: Rect,
        block: Block<'_>,
        rows: Vec<Line<'static>>,
        buf: &mut Buffer,
    ) {
        let text_area = block.inner(popup);
        let paragraph = Paragraph::new(rows.clone())
            .block(block)
            .scroll((rows_u16(self.popup_scroll), 0));
        Clear.render(popup, buf);
        Widget::render(&paragraph, popup, buf);
        self.hits.popup = Some(crate::mouse::PopupHit {
            area: popup,
            content: crate::mouse::PopupContent::Text {
                pane,
                area: text_area,
                lines: rows,
            },
        });
    }

    fn render_help(&mut self, area: Rect, buf: &mut Buffer) {
        let rows = help_rows();
        let content_width = rows.iter().map(Line::width).max().unwrap_or_default();
        let popup = centered_popup(area, content_width, rows.len());
        let vertical_chrome = usize::from(POPUP_PADDING.top + POPUP_PADDING.bottom + 2);
        let visible = usize::from(popup.height).saturating_sub(vertical_chrome);
        let max_scroll = rows.len().saturating_sub(visible);
        self.popup_scroll = self.popup_scroll.min(max_scroll);
        let mut block = Block::default()
            .borders(Borders::ALL)
            .padding(POPUP_PADDING)
            .title(" Keys ")
            .title_bottom(" Esc closes ".fg(Color::DarkGray));
        if max_scroll > 0 {
            block = block.title_bottom(self.scroll_position(visible, rows.len()));
        }
        self.render_text_popup(crate::mouse::TextPane::Help, popup, block, rows, buf);
    }

    /// Draws what the last mouse release copied at the right end of the footer's last row, where
    /// the hints leave room, so it never changes the layout; returns the row the quit hint covers
    /// instead, when there is no room for it.
    fn render_mouse_notice(&self, footer: &[HintRow], area: Rect, buf: &mut Buffer) -> Option<u16> {
        // How to quit matters more than what the last copy took.
        let quitting = self.quit_hint_at.is_some();
        let (notice, color) = if quitting {
            let action = if self.source.attachment_status().is_some() {
                "detach"
            } else {
                "quit"
            };
            (Some(format!("press q to {action}")), Color::Yellow)
        } else {
            (self.mouse.notice.clone(), Color::Green)
        };
        let (Some(notice), Some(last)) = (notice, footer.last()) else {
            return None;
        };
        let notice = format!("{notice} ");
        let width = rows_u16(notice.chars().count());
        let used = rows_u16(last.line.width()).saturating_add(2);
        if area.height == 0 {
            return None;
        }
        if used.saturating_add(width) > area.width {
            // The quit hint must show, so for its two seconds it covers the hints of the
            // footer's last row; other notices wait for room.
            if !quitting {
                return None;
            }
            let row = Rect {
                y: area.bottom() - 1,
                height: 1,
                ..area
            };
            Clear.render(row, buf);
            buf.set_stringn(
                row.x,
                row.y,
                format!(" {notice}"),
                usize::from(row.width),
                Style::default().fg(color),
            );
            return Some(row.y);
        }
        buf.set_string(
            area.right() - width,
            area.bottom() - 1,
            notice,
            Style::default().fg(color),
        );
        None
    }

    /// Draws the search prompt over the footer's last row while it is open, so it never changes
    /// the layout, and returns the row it covers.
    ///
    /// A query wider than the row shows its end, where typing happens.
    fn render_search_prompt(&self, area: Rect, buf: &mut Buffer) -> Option<u16> {
        let prompt = self.search.prompt.as_deref()?;
        if area.height == 0 {
            return None;
        }
        let row = Rect {
            y: area.bottom() - 1,
            height: 1,
            ..area
        };
        let width = usize::from(row.width);
        // The query's end that fits beside the " /" before it and the cursor after it.
        let room = width.saturating_sub(3);
        let mut used = 0;
        let mut shown_from = prompt.len();
        for (start, ch) in prompt.char_indices().rev() {
            let ch_width = prompt
                .get(start..start + ch.len_utf8())
                .map_or(0, |text| Span::raw(text).width());
            if used + ch_width > room {
                break;
            }
            used += ch_width;
            shown_from = start;
        }
        let line = Line::from(vec![
            " /".fg(Color::Yellow).bold(),
            Span::raw(prompt.get(shown_from..).unwrap_or_default()),
            Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
        ]);
        let hint = " ↵ search · Esc cancel ".fg(Color::DarkGray);
        let gap = width.saturating_sub(line.width() + hint.width());
        let mut spans = line.spans;
        if gap > 0 {
            spans.extend([Span::raw(" ".repeat(gap)), hint]);
        }
        Clear.render(row, buf);
        buf.set_line(row.x, row.y, &Line::from(spans), row.width);
        Some(row.y)
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
        crate::terminal::install_panic_hook();
        if let Err(err) = crate::terminal::take_over(self.mouse.capture, !self.pty_input_mode) {
            // The failure to take the terminal over is the one to report.
            crate::terminal::give_back().ok();
            return Err(err.into());
        }
        // Always give the terminal back, even when the event loop returns an error, so a failure
        // never leaves the user's shell in raw mode, on the alternate screen or capturing the
        // mouse.
        let result = match ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(
            std::io::stdout(),
        )) {
            Ok(terminal) => self.run(terminal).await,
            Err(err) => Err(err.into()),
        };
        let given_back = crate::terminal::give_back();
        result?;
        given_back?;
        Ok(())
    }
}

pub mod log_view {
    use ratatui::{
        buffer::Buffer,
        layout::{Constraint, Direction, Layout, Rect},
        style::{Color, Modifier, Style},
        widgets::{Clear, Paragraph, Scrollbar, ScrollbarState, StatefulWidget, Widget, Wrap},
    };

    /// Narrowest left border text worth drawing; below it the truncation marker would crowd out
    /// the text itself.
    const MIN_BORDER_DETAIL_LEFT_WIDTH: usize = 12;

    /// Border columns kept free between the left text and the right facts, so the two never read
    /// as one run.
    const BORDER_DETAIL_GAP: usize = 10;

    /// Text drawn into a pane's border.
    #[derive(Debug, Clone)]
    pub struct PaneBorders<'a> {
        /// Pane name at the top left.
        pub title: ratatui::text::Line<'a>,
        /// Pane state at the top right, such as the filters that currently hide content.
        pub status: Option<ratatui::text::Line<'static>>,
        /// Identity facts along the bottom border.
        pub detail: Option<BorderDetail>,
        /// Whether the pane has the focus, which its frame shows.
        pub focused: bool,
    }

    /// Facts drawn into a pane's bottom border.
    #[derive(Debug, Clone)]
    pub struct BorderDetail {
        /// Left-aligned text, shortened with an ellipsis to the width the right side leaves.
        pub left: ratatui::text::Span<'static>,
        /// Right-aligned facts, always drawn in full.
        pub right: ratatui::text::Line<'static>,
    }

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
            self.total_lines = 0;
            self.extend(&text.lines, wrap, width);
        }

        /// Indexes `lines` appended to the text, under the configuration the index has.
        pub(crate) fn extend(&mut self, lines: &[ratatui::text::Line<'_>], wrap: bool, width: u16) {
            self.starts.reserve(lines.len());
            for line in lines {
                self.starts.push(self.total_lines);
                let height = if wrap {
                    Paragraph::new(borrow_line(line))
                        .wrap(Wrap { trim: false })
                        .line_count(width)
                } else {
                    1
                };
                self.total_lines = self.total_lines.saturating_add(height);
            }
        }

        /// Forgets the lines `removed`, cut out of the text, in order and apart.
        pub(crate) fn remove(&mut self, removed: &[std::ops::Range<usize>]) {
            let mut places = crate::log_text::RemovedLines::new(removed);
            let mut starts = Vec::with_capacity(self.starts.len());
            let mut total_lines = 0usize;
            for (line, start) in self.starts.iter().enumerate() {
                let next = self
                    .starts
                    .get(line + 1)
                    .copied()
                    .unwrap_or(self.total_lines);
                if places.place(line).is_some() {
                    starts.push(total_lines);
                    total_lines += next - start;
                }
            }
            self.starts = starts;
            self.total_lines = total_lines;
        }

        /// Total rendered rows covered by this index.
        #[must_use]
        pub(crate) fn total_lines(&self) -> usize {
            self.total_lines
        }

        /// The rendered row that logical line `line` starts on.
        #[must_use]
        pub(crate) fn line_start(&self, line: usize) -> Option<usize> {
            self.starts.get(line).copied()
        }

        /// The logical line that rendered `row` belongs to, and the row it starts on.
        #[must_use]
        pub(crate) fn line_of_row(&self, row: usize) -> Option<(usize, usize)> {
            let line = self
                .starts
                .partition_point(|start| *start <= row)
                .checked_sub(1)?;
            Some((line, *self.starts.get(line)?))
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

    /// A search match, in reverse video of the theme's yellow, so it reads apart from a mouse
    /// selection's plain reverse video.
    const MATCH_STYLE: Style = Style::new()
        .fg(Color::Yellow)
        .add_modifier(Modifier::REVERSED);

    /// The current search match, in bold reverse video of the focus accent.
    const CURRENT_MATCH_STYLE: Style = Style::new()
        .fg(super::FOCUS_COLOR)
        .add_modifier(Modifier::REVERSED)
        .add_modifier(Modifier::BOLD);

    /// Search matches a pane draws highlighted.
    #[derive(Debug, Clone, Copy, Default)]
    pub(crate) struct Highlights<'a> {
        /// The matches, ordered by line and then by position, none overlapping.
        pub matches: &'a [crate::search::Match],
        /// The index of the match drawn as the current one.
        pub current: Option<usize>,
    }

    /// Where a pane drawn in `area` puts its text: inside its border, left of its scrollbar.
    #[must_use]
    pub(crate) fn text_area(area: Rect) -> Rect {
        Rect {
            x: area.x.saturating_add(1),
            y: area.y.saturating_add(1),
            width: area.width.saturating_sub(3),
            height: area.height.saturating_sub(2),
        }
    }

    /// The text a pane shows from rendered row `offset` on, for `height` rows.
    ///
    /// Panes draw it, and copying a selection draws it again off screen, so both wrap the same.
    pub(crate) fn content_paragraph<'a>(
        text: &'a ratatui::text::Text<'_>,
        line_index: &RenderedLineIndex,
        offset: usize,
        height: usize,
        wrap: bool,
        highlights: Highlights<'_>,
    ) -> Paragraph<'a> {
        let (text, paragraph_offset) = window_text(text, line_index, offset, height, highlights);
        let mut paragraph = Paragraph::new(text).scroll((paragraph_offset, 0));
        if wrap {
            paragraph = paragraph.wrap(Wrap { trim: false });
        }
        paragraph
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
        /// Scrolls by `rows`, up when negative, through `total` rows shown `viewport` at a time.
        ///
        /// Following the tail stops only when the view leaves the bottom, so a pane whose text
        /// fits, such as a full-screen program's live screen, keeps following however the wheel
        /// turns; scrolling down to the bottom follows again.
        pub(crate) fn scroll_by(&mut self, rows: isize, total: usize, viewport: usize) {
            let max_off = total.saturating_sub(viewport);
            let from = if self.follow_tail {
                max_off
            } else {
                self.scroll_offset.min(max_off)
            };
            let to = from.saturating_add_signed(rows).min(max_off);
            self.scroll_offset = to;
            if to != from || rows > 0 {
                self.follow_tail = to == max_off;
            }
        }

        /// Render a scrollable text pane and return its wrap-aware rendered line count, so callers
        /// can clamp keyboard scrolling consistently with the scrollbar/follow-tail behavior.
        ///
        /// `line_index` must describe `text` under this view's current wrap configuration.
        pub fn render(
            &mut self,
            area: Rect,
            line_index: &RenderedLineIndex,
            text: &ratatui::text::Text<'_>,
            borders: PaneBorders<'_>,
            highlights: Highlights<'_>,
            buf: &mut Buffer,
        ) -> usize {
            let PaneBorders {
                title,
                status,
                detail,
                focused,
            } = borders;
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

            let paragraph = content_paragraph(
                text,
                line_index,
                self.scroll_offset,
                viewport_height,
                self.wrap,
                highlights,
            );

            let mut block = super::pane_block(focused).title(title);
            if let Some(status) = status {
                block = block.title(status.right_aligned());
            }
            if let Some(BorderDetail { left, right }) = detail {
                // The corners take a column each, and the gap keeps the two sides apart.
                let left_width = usize::from(log_area.width)
                    .saturating_sub(right.width() + 2 + BORDER_DETAIL_GAP);
                if left_width >= MIN_BORDER_DETAIL_LEFT_WIDTH {
                    block = block.title_bottom(ratatui::text::Span::styled(
                        super::truncate_with_dots(&left.content, left_width),
                        left.style,
                    ));
                }
                block = block.title_bottom(right.right_aligned());
            }
            let paragraph = paragraph.block(block);

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
        highlights: Highlights<'_>,
    ) -> (ratatui::text::Text<'a>, u16) {
        let (source_lines, local_offset) = line_index.source_window(offset, viewport_height);
        (
            borrow_text_range(text, source_lines, highlights),
            local_offset,
        )
    }

    fn borrow_text_range<'a>(
        text: &'a ratatui::text::Text<'_>,
        source_lines: std::ops::Range<usize>,
        highlights: Highlights<'_>,
    ) -> ratatui::text::Text<'a> {
        let first = highlights
            .matches
            .partition_point(|found| found.line < source_lines.start);
        let mut pending = highlights.matches.iter().enumerate().skip(first).peekable();
        let lines = text
            .lines
            .get(source_lines.clone())
            .unwrap_or_default()
            .iter()
            .zip(source_lines)
            .map(|(line, index)| {
                let mut marks = Vec::new();
                while let Some((at, found)) = pending.next_if(|(_, found)| found.line == index) {
                    marks.push(Mark {
                        bytes: found.bytes.clone(),
                        style: if highlights.current == Some(at) {
                            CURRENT_MATCH_STYLE
                        } else {
                            MATCH_STYLE
                        },
                    });
                }
                if marks.is_empty() {
                    borrow_line(line)
                } else {
                    highlight_line(line, &marks)
                }
            })
            .collect();
        ratatui::text::Text {
            alignment: text.alignment,
            style: text.style,
            lines,
        }
    }

    /// Bytes of a line's text drawn in a style of their own.
    struct Mark {
        bytes: std::ops::Range<usize>,
        style: Style,
    }

    /// `line` with each of `marks`, ordered and apart, patched over the styles of the spans it
    /// covers, splitting spans where a mark starts or ends inside them.
    fn highlight_line<'a>(
        line: &'a ratatui::text::Line<'_>,
        marks: &[Mark],
    ) -> ratatui::text::Line<'a> {
        let mut spans = Vec::new();
        let mut marks = marks.iter().peekable();
        let mut span_start = 0;
        for span in &line.spans {
            let content: &'a str = span.content.as_ref();
            let span_end = span_start + content.len();
            let mut push = |from: usize, to: usize, style: Style| {
                if let Some(part) = content.get(from - span_start..to - span_start)
                    && !part.is_empty()
                {
                    spans.push(ratatui::text::Span::styled(part, style));
                }
            };
            let mut cursor = span_start;
            while cursor < span_end {
                while marks.next_if(|mark| mark.bytes.end <= cursor).is_some() {}
                match marks.peek() {
                    Some(mark) if mark.bytes.start < span_end => {
                        let start = mark.bytes.start.max(cursor);
                        let end = mark.bytes.end.min(span_end);
                        push(cursor, start, span.style);
                        push(start, end, span.style.patch(mark.style));
                        cursor = end;
                    }
                    _ => {
                        push(cursor, span_end, span.style);
                        cursor = span_end;
                    }
                }
            }
            span_start = span_end;
        }
        ratatui::text::Line {
            style: line.style,
            alignment: line.alignment,
            spans,
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

/// The marker in front of a selected row, drawn in `style`.
fn selection_marker(style: Style) -> Line<'static> {
    Line::from(vec![" ".into(), "▶".set_style(style), " ".into()])
}
