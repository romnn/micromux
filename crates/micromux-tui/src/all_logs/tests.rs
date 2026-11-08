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

use super::{AllLogs, fit_name, tag};
use crate::log_text::{RecordId, RecordKey, RecordLine};
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

/// Has the service at `index` log `records`, read from a model that retains its records from
/// `first_retained` on, for the next frame to show.
fn log_more(
    app: &mut App,
    index: usize,
    first_retained: u64,
    records: &[(u64, u64, String)],
) -> eyre::Result<()> {
    let records = records
        .iter()
        .map(|(seq, timestamp_unix_ms, text)| (*seq, *timestamp_unix_ms, text.as_str()))
        .collect::<Vec<_>>();
    apply(
        app.state
            .services
            .get_mut(index)
            .ok_or_eyre("no such service")?,
        first_retained,
        &records,
    );
    Ok(())
}

/// The text the log pane's row `y` shows, without the sidebar and the pane's borders.
fn pane_row(app: &App, frame: &Buffer, y: u16) -> String {
    let area = crate::render::log_view::text_area(app.hits.logs);
    (area.x..area.right())
        .filter_map(|x| frame.cell((x, y)).map(ratatui::buffer::Cell::symbol))
        .collect()
}

/// Selects from the start of `from` to the end of `to`, which the frame shows, with a drag.
fn select(app: &mut App, frame: &Buffer, from: &str, to: &str) -> eyre::Result<()> {
    let start = find(frame, from)?;
    let end = find(frame, to)?;
    let last = Position::new(end.x + u16::try_from(to.chars().count())? - 1, end.y);
    mouse(app, MouseEventKind::Down(MouseButton::Left), start);
    mouse(app, MouseEventKind::Drag(MouseButton::Left), last);
    mouse(app, MouseEventKind::Up(MouseButton::Left), last);
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
    Ok(record_line.record.id.service)
}

/// An [`AllLogs`] built from `services` in one go.
fn built(services: &[Service]) -> AllLogs {
    let mut all_logs = AllLogs::new();
    all_logs.render_text(services, false, 80);
    all_logs
}

/// Gives `service` the further log `records` of `(seq, timestamp_unix_ms, line)`, read from a
/// model that retains them from `first_retained` on, as reading the model does.
fn apply(service: &mut Service, first_retained: u64, records: &[(u64, u64, &str)]) {
    let format = service
        .cached_format
        .clone()
        .unwrap_or_else(|| service.line_format(false));
    service.use_format(&format);
    let records = records
        .iter()
        .map(|&(seq, timestamp_unix_ms, line)| micromux::LogLine {
            seq,
            run_generation: 1,
            timestamp_unix_ms,
            line: line.to_string(),
        })
        .collect::<Vec<_>>();
    service.apply_log_records(Some(first_retained), &records, &format);
}

#[test]
fn records_merge_by_capture_time_then_service_id_and_keep_each_services_order() {
    // The sidebar lists web first, but ties go by service ID, as follow_all_logs breaks them.
    let services = [
        service("web", &[(1, 100, "w1"), (2, 200, "w2"), (3, 300, "w3")]),
        service("api", &[(1, 100, "a1"), (2, 300, "a2"), (3, 300, "a3")]),
    ];

    assert_eq!(
        lines(&built(&services)),
        vec![
            "api │ a1",
            "web │ w1",
            "web │ w2",
            "api │ a2",
            "api │ a3",
            "web │ w3",
        ]
    );
}

#[test]
fn tags_pad_names_to_one_width_and_cut_long_ones_in_the_middle() {
    let text = |spans: Vec<ratatui::text::Span<'static>>| {
        spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };

    // A short name is padded, so the log text lines up after every tag.
    assert_eq!(text(tag("api", 0, 6)), "api    │ ");

    // Long names that share a prefix keep the ends that tell them apart.
    assert_eq!(fit_name("payments-service-api", 16), "payment…vice-api");
    assert_eq!(fit_name("payments-service-worker", 16), "payment…e-worker");
    assert_eq!(
        text(tag("payments-service-api", 0, 16)),
        "payment…vice-api │ "
    );

    // Services take the theme's colors by their number, and the colors repeat after the last.
    let color = |number| tag("api", number, 3).first().and_then(|span| span.style.fg);
    assert_eq!(color(0), Some(Color::Cyan));
    assert_eq!(color(1), Some(Color::Magenta));
    assert_eq!(
        color(u32::try_from(super::TAG_COLORS.len()).unwrap_or_default()),
        Some(Color::Cyan)
    );
}

#[test]
fn every_line_of_a_record_is_tagged_and_no_color_bleeds_into_the_next_record() {
    let services = [
        // A live screen is one record of several lines, and this one leaves red set.
        service("api", &[(1, 100, "first row\nsecond row\x1b[31m red")]),
        service("worker", &[(1, 200, "plain")]),
    ];
    let all_logs = built(&services);

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

#[test]
fn a_live_screen_redrawn_in_place_stays_at_the_time_of_its_first_frame() {
    let mut services = [service("api", &[]), service("web", &[])];
    let [api, web] = &mut services;
    apply(api, 1, &[(1, 100, "frame one")]);
    apply(web, 1, &[(1, 200, "web line")]);
    let mut all_logs = built(&services);

    // The model replaces the screen's record in place with a later frame.
    let [api, _] = &mut services;
    apply(api, 1, &[(1, 300, "frame two")]);
    all_logs.render_text(&services, false, 80);

    // The screen shows its new frame where its first one was, before the later web line.
    assert_eq!(lines(&all_logs), vec!["api │ frame two", "web │ web line"]);
    let [api, _] = &services;
    assert_eq!(
        api.cached_lines.front().map(|line| line.timestamp_unix_ms),
        Some(100)
    );
}

#[test]
fn editing_at_the_ends_builds_the_same_text_as_building_it_whole() {
    /// What the services log before one render.
    struct Step {
        api: &'static [(u64, u64, &'static str)],
        web: &'static [(u64, u64, &'static str)],
        /// The oldest record api's model retains.
        api_first_retained: u64,
    }

    let mut services = [service("api", &[]), service("web", &[])];
    let mut edited = AllLogs::new();
    let steps = [
        Step {
            api: &[(1, 100, "a1"), (2, 300, "a2")],
            web: &[(1, 200, "w1")],
            api_first_retained: 1,
        },
        Step {
            api: &[(3, 400, "a3")],
            web: &[(2, 400, "w2"), (3, 500, "w3")],
            api_first_retained: 1,
        },
        // api drops its first two lines, so the text starts at its oldest kept line.
        Step {
            api: &[(4, 600, "a4")],
            web: &[(4, 600, "w4")],
            api_first_retained: 3,
        },
        Step {
            api: &[],
            web: &[(5, 700, "w5\nsecond line")],
            api_first_retained: 3,
        },
    ];
    for (step, logged) in steps.iter().enumerate() {
        let [api, web] = &mut services;
        apply(api, logged.api_first_retained, logged.api);
        apply(web, 1, logged.web);
        edited.render_text(&services, false, 80);

        // After the first build, each step edits the text at its ends.
        assert_eq!(edited.log.last_edit.is_some(), step > 0, "step {step}");
        let whole = built(&services);
        assert_eq!(lines(&edited), lines(&whole), "step {step}");
        assert_eq!(
            edited.log.line_index.total_lines(),
            whole.log.line_index.total_lines()
        );
        // Every line names the same record in both.
        for line in 0..whole.log.text.lines.len() {
            let record_line = whole.log.record_line(line);
            assert_eq!(edited.log.record_line(line), record_line, "step {step}");
            if let Some(record_line) = record_line {
                assert_eq!(edited.log.find_record_line(record_line), Ok(line));
            }
        }
    }
    assert_eq!(
        lines(&edited),
        vec![
            "api │ a3",
            "web │ w2",
            "web │ w3",
            "api │ a4",
            "web │ w4",
            "web │ w5",
            "web │ second line",
        ]
    );
}

#[test]
fn a_replaced_record_or_a_new_line_out_of_order_builds_the_text_whole() {
    let mut services = [service("api", &[]), service("web", &[])];
    let [api, web] = &mut services;
    apply(api, 1, &[(1, 100, "a1")]);
    apply(web, 1, &[(1, 300, "w1")]);
    let mut all_logs = built(&services);

    // A line captured before the last one in the text must go before it.
    let [api, _] = &mut services;
    apply(api, 1, &[(2, 200, "a2")]);
    all_logs.render_text(&services, false, 80);
    assert_eq!(all_logs.log.last_edit, None);
    assert_eq!(lines(&all_logs), vec!["api │ a1", "api │ a2", "web │ w1"]);

    // A record replaced in place changes the middle of the text.
    let [api, _] = &mut services;
    apply(api, 1, &[(2, 200, "a2 redrawn")]);
    all_logs.render_text(&services, false, 80);
    assert_eq!(all_logs.log.last_edit, None);
    assert_eq!(
        lines(&all_logs),
        vec!["api │ a1", "api │ a2 redrawn", "web │ w1"]
    );
}

#[test]
fn lines_older_than_the_oldest_line_a_service_kept_are_left_out() {
    let mut services = [service("api", &[]), service("web", &[])];
    let [api, web] = &mut services;
    // api's model no longer has its first four records.
    apply(
        api,
        5,
        &[(5, 300, "api oldest kept"), (6, 500, "api newer")],
    );
    apply(
        web,
        1,
        &[
            (1, 100, "web old"),
            (2, 200, "web older than api"),
            (3, 400, "web"),
        ],
    );

    // Before api's oldest kept line, its lines would be missing, so web's are left out too.
    assert_eq!(
        lines(&built(&services)),
        vec!["api │ api oldest kept", "web │ web", "api │ api newer"]
    );
}

#[test]
fn a_record_is_found_by_its_service_and_sequence_however_the_roster_changes() {
    let services = [
        service("api", &[(1, 100, "a1")]),
        service("web", &[(1, 100, "w1")]),
    ];
    let mut all_logs = built(&services);
    let web_line = RecordLine {
        record: RecordKey {
            id: RecordId { service: 1, seq: 1 },
            timestamp_unix_ms: 100,
        },
        line: 0,
    };
    assert_eq!(all_logs.log.find_record_line(web_line), Ok(1));

    // A service that sorts first joins at the top of the sidebar.
    let services = [
        service("aaa", &[(1, 50, "x1")]),
        service("api", &[(1, 100, "a1")]),
        service("web", &[(1, 100, "w1")]),
    ];
    all_logs.dirty = true;
    all_logs.render_text(&services, false, 80);

    // web keeps its number, and its line is found where it moved.
    assert_eq!(all_logs.log.find_record_line(web_line), Ok(2));
    assert_eq!(lines(&all_logs), vec!["aaa │ x1", "api │ a1", "web │ w1"]);
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
    let top = pane_row(&app, &frame, text_area.y);
    assert!(top.contains("line 16"), "{top}");

    // Each service drops its ten oldest lines while ten new ones arrive.
    log_more(&mut app, 0, 11, &numbered("api", 41..=50, 0))?;
    log_more(&mut app, 1, 11, &numbered("web", 41..=50, 5))?;
    draw(&mut app);

    // The text was edited at its ends, and the same line still tops the view, although the
    // lines above it are gone.
    let all_logs = app
        .state
        .all_logs
        .as_ref()
        .ok_or_eyre("all logs are hidden")?;
    assert!(all_logs.log.last_edit.is_some());
    let frame = draw(&mut app);
    assert_eq!(pane_row(&app, &frame, text_area.y), top);
    Ok(())
}

#[tokio::test]
async fn a_selection_and_a_scrolled_view_stay_while_a_service_joins() -> eyre::Result<()> {
    let mut app = all_logs_app()?;
    set_lines(&mut app, 0, &numbered("api", 1..=40, 0))?;
    set_lines(&mut app, 1, &numbered("web", 1..=40, 5))?;
    app.log_view.follow_tail = false;
    app.log_view.scroll_offset = 30;
    let frame = draw(&mut app);
    let text_area = crate::render::log_view::text_area(app.hits.logs);
    let top = pane_row(&app, &frame, text_area.y);
    select(&mut app, &frame, "api │ api line 18", "web │ web line 18")?;
    let selected = app.selected_text();
    assert_eq!(
        selected.as_deref(),
        Some("api │ api line 18\nweb │ web line 18")
    );

    // A service that sorts first joins at the top of the sidebar, as a reconciled config or a
    // dynamic service adds one, with lines among the others' though none inside the selection.
    let mut joined = Service::new(micromux::ServiceSnapshot::initial(
        "aaa".to_string(),
        "aaa".to_string(),
        Vec::new(),
        None,
        micromux::RestartPolicy::Never,
        Vec::new(),
        None,
    ));
    joined.cached_format = Some(joined.line_format(app.pretty_json_logs));
    joined.logs_dirty = false;
    joined.cached_lines = cached(&numbered("aaa", 1..=40, 7));
    app.state.services.insert(0, joined);
    if let Some(all_logs) = &mut app.state.all_logs {
        all_logs.dirty = true;
    }

    // The view still starts at the same line, and the selection still holds the same text.
    let frame = draw(&mut app);
    assert_eq!(pane_row(&app, &frame, text_area.y), top);
    assert_eq!(app.selected_text(), selected);
    Ok(())
}

#[tokio::test]
async fn changing_a_services_format_drops_a_selection_in_all_logs() -> eyre::Result<()> {
    let mut app = all_logs_app()?;
    set_lines(&mut app, 0, &[(1, 100, "api first")])?;
    set_lines(&mut app, 1, &[(1, 200, "web second")])?;
    let frame = draw(&mut app);
    select(&mut app, &frame, "api │ api first", "web │ web second")?;
    assert!(app.selected_text().is_some());

    // Toggling the selected service's timestamps may change what its lines say.
    press(&mut app, KeyCode::Char('T'));
    draw(&mut app);
    assert_eq!(app.selected_text(), None);
    Ok(())
}

#[tokio::test]
async fn a_search_takes_in_new_lines_and_keeps_its_current_match() -> eyre::Result<()> {
    let mut app = all_logs_app()?;
    set_lines(&mut app, 0, &[(1, 100, "api needle")])?;
    set_lines(&mut app, 1, &[(1, 200, "web needle")])?;
    search(&mut app, "needle");
    assert!(screen(&draw(&mut app)).contains("/needle 2/2"));

    // A new line is appended to the text, and only it is searched.
    log_more(&mut app, 0, 1, &[(2, 300, "api needle again".to_string())])?;
    draw(&mut app);
    let frame = draw(&mut app);
    let all_logs = app
        .state
        .all_logs
        .as_ref()
        .ok_or_eyre("all logs are hidden")?;
    assert!(all_logs.log.last_edit.is_some());
    assert_eq!(
        app.search.highlights().matches,
        crate::search::find_matches(
            &all_logs.log.text,
            &crate::search::Query::new("needle").ok_or_eyre("empty query")?
        )
        .as_slice()
    );

    // The current match stays on web's line, now the second of three.
    assert!(screen(&frame).contains("/needle 2/3"));
    assert_eq!(current_match_service(&app)?, 1);
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
