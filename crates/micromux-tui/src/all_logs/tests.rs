use std::path::Path;
use std::time::Instant;

use color_eyre::eyre::{self, OptionExt as _};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use indoc::indoc;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Color;
use ratatui::widgets::Widget as _;
use similar_asserts::assert_eq;

use super::{AllLogs, merge, tag};
use crate::state::{CachedLine, Service};
use crate::{App, Effect, LocalSource, SessionSource, ViewOptions};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 30;

const SERVICES: &str = indoc! {r#"
    version: 1
    services:
      api:
        command: ["sleep", "30"]
      web:
        command: ["sleep", "30"]
"#};

fn service(id: &str, lines: &[(u64, u64, &str)]) -> Service {
    let mut service = Service::new(micromux::ServiceSnapshot::initial(
        id.to_string(),
        id.to_string(),
        Vec::new(),
        None,
        micromux::RestartPolicy::Never,
        Vec::new(),
        None,
    ));
    service.cached_lines = cached(lines);
    service
}

/// Cached lines from `(seq, timestamp_unix_ms, text)`.
fn cached(lines: &[(u64, u64, impl AsRef<str>)]) -> std::collections::VecDeque<CachedLine> {
    lines
        .iter()
        .map(|(seq, timestamp_unix_ms, text)| CachedLine {
            seq: *seq,
            timestamp_unix_ms: *timestamp_unix_ms,
            text: text.as_ref().to_string(),
        })
        .collect()
}

/// The lines `seqs` of `service`, which logs one every 10 ms from `offset_ms` on.
fn numbered(
    service: &str,
    seqs: std::ops::RangeInclusive<u64>,
    offset_ms: u64,
) -> Vec<(u64, u64, String)> {
    seqs.map(|seq| {
        (
            seq,
            seq * 10 + offset_ms,
            format!("{service} line {seq:02}"),
        )
    })
    .collect()
}

/// The text of every line of `log`, its spans' contents joined.
fn lines(all_logs: &AllLogs) -> Vec<String> {
    all_logs
        .log
        .text
        .lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect()
}

/// An app over `yaml`'s services that has drawn one frame, and the session's runner, which
/// keeps the scheduler's input queue open while it is held, though it never runs.
fn app_and_runner(
    yaml: &str,
) -> eyre::Result<(
    App,
    impl std::future::Future<Output = Result<(), micromux::Error>>,
)> {
    let mut diagnostics = Vec::new();
    let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)?;
    // The run-log spool goes to a directory of the test's own, never the user's cache.
    // The runner never runs, so nothing writes to it after it is removed with this function.
    let cache = tempfile::tempdir()?;
    let mux =
        std::sync::Arc::new(micromux::Micromux::new(&parsed)?.with_cache_directory(cache.path()));
    let (runner, handles) = mux.start(micromux::CancellationToken::new());
    let mut app = App::new(
        SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
        Some(handles.terminal),
        micromux::CancellationToken::new(),
        ViewOptions::default(),
    );
    app.terminal_cols = WIDTH;
    app.terminal_rows = HEIGHT;
    draw(&mut app);
    Ok((app, runner))
}

/// An app over [`SERVICES`] that shows all logs and has drawn a frame of them.
fn all_logs_app() -> eyre::Result<App> {
    let (mut app, _) = app_and_runner(SERVICES)?;
    press(&mut app, KeyCode::Char('A'));
    draw(&mut app);
    Ok(app)
}

/// Gives the service at `index` the lines `(seq, timestamp_unix_ms, text)` as its log, as if it
/// had printed them, and draws a frame.
fn set_lines(
    app: &mut App,
    index: usize,
    lines: &[(u64, u64, impl AsRef<str>)],
) -> eyre::Result<()> {
    let service = app
        .state
        .services
        .get_mut(index)
        .ok_or_eyre("no such service")?;
    // The frames so far read the model's log already, so these lines are not overwritten.
    service.logs_dirty = false;
    service.cached_lines = cached(lines);
    service.text_dirty = true;
    if let Some(all_logs) = &mut app.state.all_logs {
        all_logs.dirty = true;
    }
    draw(app);
    Ok(())
}

fn draw(app: &mut App) -> Buffer {
    let area = Rect::new(0, 0, WIDTH, HEIGHT);
    let mut buffer = Buffer::empty(area);
    app.render(area, &mut buffer);
    buffer
}

fn row_text(buffer: &Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .filter_map(|x| buffer.cell((x, y)).map(ratatui::buffer::Cell::symbol))
        .collect()
}

fn screen(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| row_text(buffer, y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The screen cell where `needle` is drawn.
fn find(buffer: &Buffer, needle: &str) -> eyre::Result<Position> {
    for y in 0..buffer.area.height {
        let row = row_text(buffer, y);
        if let Some(byte) = row.find(needle) {
            let column = row.get(..byte).map_or(0, |prefix| prefix.chars().count());
            return Ok(Position::new(u16::try_from(column)?, y));
        }
    }
    eyre::bail!("{needle:?} is not on screen")
}

fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.handle_key_press(KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    });
}

fn press(app: &mut App, code: KeyCode) {
    let modifiers = match code {
        KeyCode::Char(ch) if ch.is_uppercase() => KeyModifiers::SHIFT,
        _ => KeyModifiers::NONE,
    };
    key(app, code, modifiers);
}

fn search(app: &mut App, query: &str) {
    press(app, KeyCode::Char('/'));
    for ch in query.chars() {
        press(app, KeyCode::Char(ch));
    }
    press(app, KeyCode::Enter);
}

fn mouse(app: &mut App, kind: MouseEventKind, position: Position) {
    app.handle_mouse(
        MouseEvent {
            kind,
            column: position.x,
            row: position.y,
            modifiers: KeyModifiers::NONE,
        },
        Instant::now(),
    );
}

/// Copies the selection with `y`, as a user does, and returns what was copied.
fn yank(app: &mut App) -> Vec<String> {
    press(app, KeyCode::Char('y'));
    std::mem::take(&mut app.effects)
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Copy(text) => Some(text),
            _ => None,
        })
        .collect()
}

/// The service of the current search match's line, by its place in the sidebar.
fn current_match_service(app: &App) -> eyre::Result<u32> {
    let highlights = app.search.highlights();
    let current = highlights.current.ok_or_eyre("no match is current")?;
    let found = highlights
        .matches
        .get(current)
        .ok_or_eyre("the current match is gone")?;
    let all_logs = app
        .state
        .all_logs
        .as_ref()
        .ok_or_eyre("all logs are hidden")?;
    let record_line = all_logs
        .log
        .record_line(found.line)
        .ok_or_eyre("the match has no record")?;
    Ok(record_line.record.service)
}

#[test]
fn records_merge_by_capture_time_then_sidebar_order_and_keep_each_services_order() {
    let services = [
        // The last record claims an earlier time than the one before it, as after the clock
        // was set back, yet stays after it.
        service("api", &[(1, 100, "a1"), (2, 300, "a2"), (3, 250, "a3")]),
        service("web", &[(1, 100, "w1"), (2, 200, "w2"), (3, 300, "w3")]),
    ];

    let merged = merge(&services)
        .map(|record| (record.text, record.key.timestamp_unix_ms))
        .collect::<Vec<_>>();

    // Records of the same millisecond go in sidebar order, api before web.
    // The record that went back in time is merged at the time of the one before it, so each
    // service's records keep their order and the keys stay ordered for lookups.
    assert_eq!(
        merged,
        vec![
            ("a1", 100),
            ("w1", 100),
            ("w2", 200),
            ("a2", 300),
            ("a3", 300),
            ("w3", 300),
        ]
    );
    let keys = merge(&services)
        .map(|record| record.key)
        .collect::<Vec<_>>();
    assert!(keys.is_sorted());
}

#[test]
fn tags_pad_names_to_one_width_and_cut_long_ones_short() {
    let text = |spans: Vec<ratatui::text::Span<'static>>| {
        spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };

    // A short name is padded, so the log text lines up after every tag.
    assert_eq!(text(tag("api", 0, 6)), "api    │ ");

    // A long name is cut to the width, an ellipsis taking the last column.
    assert_eq!(text(tag("background-worker", 0, 6)), "backg… │ ");

    // Services take the theme's colors in sidebar order, and the colors repeat after the last.
    let color = |index| tag("api", index, 3).first().and_then(|span| span.style.fg);
    assert_eq!(color(0), Some(Color::Cyan));
    assert_eq!(color(1), Some(Color::Magenta));
    assert_eq!(color(super::TAG_COLORS.len()), Some(Color::Cyan));
}

#[test]
fn every_line_of_a_record_is_tagged_and_no_color_bleeds_into_the_next_record() {
    let services = [
        // A live screen is one record of several lines, and this one leaves red set.
        service("api", &[(1, 100, "first row\nsecond row\x1b[31m red")]),
        service("worker", &[(1, 200, "plain")]),
    ];
    let mut all_logs = AllLogs::new();

    all_logs.render_text(&services, false, 80);

    // Every line carries its service's tag, padded to the longest name.
    assert_eq!(
        lines(&all_logs),
        vec![
            "api    │ first row",
            "api    │ second row red",
            "worker │ plain",
        ]
    );
    // The next record starts without the red the screen left set.
    let plain = all_logs
        .log
        .text
        .lines
        .get(2)
        .and_then(|line| line.spans.last())
        .map(|span| span.style.fg);
    assert_eq!(plain, Some(None));
}

#[tokio::test]
async fn shift_a_shows_every_services_logs_interleaved_and_toggles_back() -> eyre::Result<()> {
    let mut app = all_logs_app()?;
    set_lines(&mut app, 0, &[(1, 100, "api first"), (2, 300, "api third")])?;
    set_lines(&mut app, 1, &[(1, 200, "web second")])?;
    let frame = draw(&mut app);

    // The pane says what it shows, and the header that it is on.
    let shown = screen(&frame);
    assert!(shown.contains("All logs"), "{shown}");
    assert!(shown.contains("All:ON"), "{shown}");

    // Lines of both services interleave by time, each tagged with its service.
    let rows = ["api │ api first", "web │ web second", "api │ api third"]
        .into_iter()
        .map(|line| find(&frame, line).map(|position| position.y))
        .collect::<eyre::Result<Vec<_>>>()?;
    assert!(rows.is_sorted(), "{rows:?}");

    // Pressing it again shows only the selected service's logs, untagged.
    press(&mut app, KeyCode::Char('A'));
    let shown = screen(&draw(&mut app));
    assert!(shown.contains("All:OFF"), "{shown}");
    assert!(shown.contains("api first"), "{shown}");
    assert!(!shown.contains("web second"), "{shown}");
    assert!(!shown.contains("api │"), "{shown}");
    assert!(app.state.all_logs.is_none());
    Ok(())
}

#[tokio::test]
async fn a_search_finds_and_walks_the_matches_of_every_service() -> eyre::Result<()> {
    let mut app = all_logs_app()?;
    set_lines(
        &mut app,
        0,
        &[
            (1, 100, "api needle"),
            (2, 200, "api line"),
            (3, 400, "api needle"),
        ],
    )?;
    set_lines(&mut app, 1, &[(1, 300, "web needle")])?;

    // The search starts at the newest match, api's, and goes back in time across services.
    search(&mut app, "needle");
    assert!(screen(&draw(&mut app)).contains("/needle 3/3"));
    assert_eq!(current_match_service(&app)?, 0);
    press(&mut app, KeyCode::Char('n'));
    assert!(screen(&draw(&mut app)).contains("/needle 2/3"));
    assert_eq!(current_match_service(&app)?, 1);

    // Showing one service's logs again searches only those.
    press(&mut app, KeyCode::Char('A'));
    assert!(screen(&draw(&mut app)).contains("/needle 2 matches"));
    Ok(())
}

#[tokio::test]
async fn the_view_stays_on_its_line_while_services_drop_old_lines() -> eyre::Result<()> {
    let mut app = all_logs_app()?;
    set_lines(&mut app, 0, &numbered("api", 1..=40, 0))?;
    set_lines(&mut app, 1, &numbered("web", 1..=40, 5))?;

    // Scroll up off the tail so a line in the middle of the log tops the view.
    app.log_view.follow_tail = false;
    app.log_view.scroll_offset = 30;
    let frame = draw(&mut app);
    let text_area = crate::render::log_view::text_area(app.hits.logs);
    let top = row_text(&frame, text_area.y);
    assert!(top.contains("line 16"), "{top}");

    // Each service drops its ten oldest lines while ten new ones arrive.
    app.state
        .services
        .get_mut(0)
        .ok_or_eyre("no api service")?
        .cached_lines = cached(&numbered("api", 11..=50, 0));
    set_lines(&mut app, 1, &numbered("web", 11..=50, 5))?;

    // The same line still tops the view, although twenty lines above it are gone.
    let frame = draw(&mut app);
    assert_eq!(row_text(&frame, text_area.y), top);
    Ok(())
}

#[tokio::test]
async fn pty_input_mode_shows_the_selected_service_until_it_ends() -> eyre::Result<()> {
    let (mut app, _runner) = app_and_runner(SERVICES)?;
    press(&mut app, KeyCode::Char('A'));
    press(&mut app, KeyCode::Down);
    draw(&mut app);

    // Typing goes to the selected service, whose logs the pane shows meanwhile.
    press(&mut app, KeyCode::Char('a'));
    assert!(app.pty_input_mode);
    assert!(app.state.all_logs.is_none());
    let shown = screen(&draw(&mut app));
    assert!(shown.contains("Logs"), "{shown}");
    assert!(!shown.contains("All logs"), "{shown}");

    // Shift+A is typed into the service rather than switching the pane.
    press(&mut app, KeyCode::Char('A'));
    assert!(app.state.all_logs.is_none());

    // Leaving the mode shows every service's logs again.
    key(&mut app, KeyCode::Char(']'), KeyModifiers::CONTROL);
    assert!(!app.pty_input_mode);
    assert!(app.state.all_logs.is_some());
    assert!(screen(&draw(&mut app)).contains("All logs"));
    Ok(())
}

#[tokio::test]
async fn a_selection_copies_tagged_lines_and_stays_while_another_service_is_selected()
-> eyre::Result<()> {
    let mut app = all_logs_app()?;
    set_lines(&mut app, 0, &[(1, 100, "api first")])?;
    set_lines(&mut app, 1, &[(1, 200, "web second")])?;
    let frame = draw(&mut app);
    let start = find(&frame, "api │ api first")?;
    let end = find(&frame, "web │ web second")?;

    // A drag over both lines copies them with their tags, as the pane shows them.
    mouse(&mut app, MouseEventKind::Down(MouseButton::Left), start);
    let last = Position::new(end.x + 15, end.y);
    mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), last);
    mouse(&mut app, MouseEventKind::Up(MouseButton::Left), last);
    assert_eq!(
        yank(&mut app),
        vec!["api │ api first\nweb │ web second".to_string()]
    );

    // Selecting another service leaves the pane as it is, and so the selection.
    app.focus = crate::Focus::Services;
    press(&mut app, KeyCode::Down);
    draw(&mut app);
    assert_eq!(
        app.selected_text().as_deref(),
        Some("api │ api first\nweb │ web second")
    );
    Ok(())
}
