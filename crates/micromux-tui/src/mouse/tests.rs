use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use color_eyre::eyre;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use indoc::indoc;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::Modifier;
use ratatui::widgets::Widget as _;
use similar_asserts::assert_eq;

use crate::{App, Effect, Focus, LocalSource, SessionSource, ViewOptions};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 30;

const SERVICES: &str = indoc! {r#"
    version: 1
    services:
      api:
        command: ["sleep", "30"]
      web:
        command: ["sleep", "30"]
      worker:
        command: ["sleep", "30"]
"#};

/// An app over `yaml`'s services that has drawn one frame, with mouse capture on.
fn app(yaml: &str) -> eyre::Result<App> {
    let mut diagnostics = Vec::new();
    let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)?;
    let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
    let (_runner, handles) = mux.start(micromux::CancellationToken::new());
    let mut app = App::new(
        SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
        Some(handles.terminal),
        micromux::CancellationToken::new(),
        ViewOptions::default(),
    );
    app.terminal_cols = WIDTH;
    app.terminal_rows = HEIGHT;
    draw(&mut app);
    Ok(app)
}

/// Gives the selected service `lines` as its log, as if it had printed them.
fn set_logs(app: &mut App, lines: impl IntoIterator<Item = String>) -> eyre::Result<()> {
    let service = app
        .state
        .current_service_mut()
        .ok_or_else(|| eyre::eyre!("no service is selected"))?;
    // The first frame read the model's log already, so these lines are not overwritten.
    service.logs_dirty = false;
    service.cached_lines = lines
        .into_iter()
        .enumerate()
        .map(|(seq, line)| (u64::try_from(seq).unwrap_or(u64::MAX), line))
        .collect();
    service.text_dirty = true;
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

/// The screen cell where `needle` is drawn, found in the frame itself rather than the hit map.
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

fn mouse(app: &mut App, kind: MouseEventKind, position: Position) {
    app.handle_mouse(MouseEvent {
        kind,
        column: position.x,
        row: position.y,
        modifiers: KeyModifiers::NONE,
    });
}

fn click(app: &mut App, position: Position) {
    mouse(app, MouseEventKind::Down(MouseButton::Left), position);
    mouse(app, MouseEventKind::Up(MouseButton::Left), position);
}

/// Presses at `from`, drags through `through` in order, and releases at the last place.
fn drag(app: &mut App, from: Position, through: &[Position]) {
    mouse(app, MouseEventKind::Down(MouseButton::Left), from);
    let mut last = from;
    for &position in through {
        mouse(app, MouseEventKind::Drag(MouseButton::Left), position);
        last = position;
    }
    mouse(app, MouseEventKind::Up(MouseButton::Left), last);
}

fn press_key(app: &mut App, code: KeyCode) {
    app.handle_key_press(KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    });
}

fn copied(app: &mut App) -> Vec<String> {
    std::mem::take(&mut app.effects)
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::Copy(text) => Some(text),
            _ => None,
        })
        .collect()
}

fn numbered(lines: usize) -> impl Iterator<Item = String> {
    (0..lines).map(|line| format!("line {line} of the log"))
}

#[tokio::test]
async fn clicks_select_services_and_focus_the_pane_under_them() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    press_key(&mut app, KeyCode::Char('H'));
    let frame = draw(&mut app);

    // A click on a service's name selects it and focuses the services pane.
    app.focus = Focus::Logs;
    click(&mut app, find(&frame, "worker")?);
    assert_eq!(app.state.selected_service, 2);
    assert_eq!(app.focus, Focus::Services);

    // A click anywhere in another pane focuses it, as Tab would, without changing the service.
    let logs = find(&frame, "Logs")?;
    click(&mut app, Position::new(logs.x + 4, logs.y + 3));
    assert_eq!(app.focus, Focus::Logs);
    let health = find(&frame, "Healthcheck")?;
    click(&mut app, Position::new(health.x + 2, health.y + 3));
    assert_eq!(app.focus, Focus::Healthcheck);
    assert_eq!(app.state.selected_service, 2);

    // A click below the last service focuses the list but selects nothing new.
    let below = find(&frame, "worker")?;
    click(&mut app, Position::new(below.x, below.y + 2));
    assert_eq!(app.state.selected_service, 2);
    assert_eq!(app.focus, Focus::Services);
    Ok(())
}

#[tokio::test]
async fn service_clicks_follow_the_list_when_it_scrolls() -> eyre::Result<()> {
    let mut services = String::new();
    for index in 0..40 {
        writeln!(
            services,
            "  svc{index:02}:\n    command: [\"sleep\", \"30\"]"
        )?;
    }
    let mut app = app(&format!("version: 1\nservices:\n{services}"))?;

    // Selecting the last service scrolls the list so it shows.
    app.state.selected_service = 39;
    let frame = draw(&mut app);
    assert!(app.hits.first_service > 0);

    // A click on a row picks the service drawn there, not the one at that index from the top.
    click(&mut app, find(&frame, "svc30")?);
    assert_eq!(
        app.state
            .current_service()
            .map(|service| service.snapshot.id.as_str()),
        Some("svc30")
    );
    Ok(())
}

#[tokio::test]
async fn the_wheel_scrolls_the_pane_under_the_pointer_and_follows_again_at_the_bottom()
-> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_logs(&mut app, numbered(100))?;
    let frame = draw(&mut app);
    let logs = find(&frame, "Logs")?;
    let in_logs = Position::new(logs.x + 4, logs.y + 4);
    let bottom = app.log_view.scroll_offset;
    assert!(app.log_view.follow_tail);

    // Turning the wheel up leaves follow mode and scrolls three rows, without moving the focus.
    mouse(&mut app, MouseEventKind::ScrollUp, in_logs);
    mouse(&mut app, MouseEventKind::ScrollUp, in_logs);
    assert!(!app.log_view.follow_tail);
    assert_eq!(app.log_view.scroll_offset, bottom - 6);
    assert_eq!(app.focus, Focus::Services);

    // Turning it back down to the bottom follows the tail again.
    mouse(&mut app, MouseEventKind::ScrollDown, in_logs);
    assert!(!app.log_view.follow_tail);
    mouse(&mut app, MouseEventKind::ScrollDown, in_logs);
    assert_eq!(app.log_view.scroll_offset, bottom);
    assert!(app.log_view.follow_tail);

    // Over the services, the wheel moves the selection.
    let services = find(&frame, "web")?;
    mouse(&mut app, MouseEventKind::ScrollDown, services);
    assert_eq!(app.state.selected_service, 1);
    mouse(&mut app, MouseEventKind::ScrollUp, services);
    assert_eq!(app.state.selected_service, 0);
    Ok(())
}

#[tokio::test]
async fn a_drag_selects_text_inside_one_pane_and_copies_it_on_release() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_logs(&mut app, numbered(10))?;
    let frame = draw(&mut app);
    let start = find(&frame, "line 2 of")?;
    let end = find(&frame, "line 4 of")?;

    // From "of" on the line 2 to "line 4": the middle line is whole, the ends are partial.
    drag(
        &mut app,
        Position::new(start.x + 7, start.y),
        &[Position::new(end.x + 5, end.y)],
    );
    assert_eq!(
        copied(&mut app),
        vec!["of the log\nline 3 of the log\nline 4".to_string()]
    );
    assert_eq!(app.mouse.notice.as_deref(), Some("copied 3 lines"));

    // The selection stays highlighted, on the selected cells only.
    let frame = draw(&mut app);
    let reversed = |position: Position| {
        frame
            .cell(position)
            .is_some_and(|cell| cell.modifier.contains(Modifier::REVERSED))
    };
    assert!(reversed(Position::new(start.x + 7, start.y)));
    assert!(reversed(Position::new(end.x + 5, end.y)));
    assert!(!reversed(Position::new(end.x + 6, end.y)));
    assert!(!reversed(Position::new(start.x + 6, start.y)));
    // A row inside the selection is highlighted to the end of its text, not to the pane's edge.
    assert!(reversed(Position::new(start.x + 16, start.y + 1)));
    assert!(!reversed(Position::new(start.x + 17, start.y + 1)));

    // A drag that leaves the pane stays in it: across the services list, the selection runs to
    // the start of the row, and nothing of the list is copied.
    let services = find(&frame, "worker")?;
    drag(
        &mut app,
        Position::new(end.x + 4, end.y),
        &[Position::new(services.x, end.y)],
    );
    assert_eq!(copied(&mut app), vec!["line ".to_string()]);

    // Esc drops the selection, and a plain click copies nothing.
    press_key(&mut app, KeyCode::Esc);
    assert!(app.mouse.selection.is_none());
    click(&mut app, Position::new(start.x + 1, start.y));
    assert_eq!(copied(&mut app), Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn double_and_triple_clicks_select_a_word_and_a_whole_wrapped_line() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    let long = format!("start {} end", "x".repeat(150));
    set_logs(
        &mut app,
        ["short line".to_string(), long.clone(), "after".to_string()],
    )?;
    press_key(&mut app, KeyCode::Char('w'));
    let frame = draw(&mut app);
    let short = find(&frame, "short line")?;
    let wrapped = find(&frame, "start")?;
    let now = Instant::now();

    // A second press on the same cell selects the word there.
    app.press(Position::new(short.x + 7, short.y), now);
    app.press(
        Position::new(short.x + 7, short.y),
        now + Duration::from_millis(100),
    );
    app.release();
    assert_eq!(copied(&mut app), vec!["line".to_string()]);

    // A third selects the whole line, and the rows of a wrapped line join without a break.
    for offset in 0..3 {
        app.press(
            Position::new(wrapped.x + 1, wrapped.y),
            now + Duration::from_millis(1000 + 100 * offset),
        );
    }
    app.release();
    assert_eq!(copied(&mut app), vec![long]);

    // A press that comes too late starts over.
    app.press(Position::new(short.x, short.y), now);
    app.press(
        Position::new(short.x, short.y),
        now + Duration::from_secs(1),
    );
    app.release();
    assert_eq!(copied(&mut app), Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn a_drag_past_the_pane_scrolls_it_and_keeps_selecting() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_logs(&mut app, numbered(100))?;
    let frame = draw(&mut app);
    // The title starts in the first column of text.
    let logs = find(&frame, "Logs")?;
    let first_row = Position::new(logs.x, logs.y + 1);
    let top = app.log_view.scroll_offset;

    // Dragging above the pane scrolls up a row per event and extends the selection to the top
    // row drawn, which is now an earlier line.
    let above = Position::new(logs.x + 2, logs.y.saturating_sub(1));
    drag(&mut app, first_row, &[above, above]);
    assert_eq!(app.log_view.scroll_offset, top - 2);
    assert!(!app.log_view.follow_tail);
    let copied = copied(&mut app);
    assert_eq!(copied.len(), 1);
    let text = copied.first().map(String::as_str).unwrap_or_default();
    assert_eq!(
        text,
        format!("ne {} of the log\nline {} of the log\nl", top - 2, top - 1)
    );
    Ok(())
}

#[tokio::test]
async fn a_press_outside_a_popup_closes_it_and_a_press_inside_does_not() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    let outside = Position::new(1, HEIGHT / 2);

    // Help
    press_key(&mut app, KeyCode::Char('?'));
    let frame = draw(&mut app);
    click(&mut app, find(&frame, "Log level threshold")?);
    assert!(app.show_help);
    click(&mut app, outside);
    assert!(!app.show_help);
    // The press that closed the popup did not also act behind it.
    assert_eq!(app.focus, Focus::Services);
    assert_eq!(app.state.selected_service, 0);

    // Details, whose text can be selected and copied
    press_key(&mut app, KeyCode::Char('i'));
    let frame = draw(&mut app);
    let state = find(&frame, "State")?;
    drag(&mut app, state, &[Position::new(state.x + 4, state.y)]);
    assert_eq!(copied(&mut app), vec!["State".to_string()]);
    assert!(app.info.is_some());
    click(&mut app, outside);
    assert!(app.info.is_none());

    // The level picker applies the choice clicked.
    press_key(&mut app, KeyCode::Char('L'));
    let frame = draw(&mut app);
    click(&mut app, find(&frame, "ERROR")?);
    assert!(app.level_picker.is_none());
    assert_eq!(
        app.state
            .current_service()
            .map(crate::state::Service::min_level),
        Some(Some(micromux::StructuredLogLevel::Error))
    );
    Ok(())
}

#[tokio::test]
async fn the_wheel_scrolls_an_open_popup_wherever_the_pointer_is() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    // A short terminal leaves the help less room than it needs.
    app.terminal_rows = 16;
    press_key(&mut app, KeyCode::Char('?'));
    let area = Rect::new(0, 0, WIDTH, 16);
    let mut frame = Buffer::empty(area);
    (&mut app).render(area, &mut frame);
    assert!((0..16).any(|y| row_text(&frame, y).contains("↑/↓ scroll")));

    mouse(&mut app, MouseEventKind::ScrollDown, Position::new(1, 1));
    assert_eq!(app.popup_scroll, 3);
    mouse(&mut app, MouseEventKind::ScrollUp, Position::new(1, 1));
    assert_eq!(app.popup_scroll, 0);
    // The logs behind it did not move.
    assert!(app.log_view.follow_tail);
    Ok(())
}

#[tokio::test]
async fn hints_run_their_key_but_never_quit() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    let frame = draw(&mut app);

    // Header and footer hints act like their keys.
    click(&mut app, find(&frame, "H Health")?);
    assert!(app.show_healthcheck_pane);
    click(&mut app, find(&frame, "Tab Focus")?);
    assert_eq!(app.focus, Focus::Logs);
    click(&mut app, find(&frame, "? Help")?);
    assert!(app.show_help);
    press_key(&mut app, KeyCode::Esc);

    // Until a frame shows the popup that a click opened, further mouse events are ignored rather
    // than landing on what the popup now covers.
    let frame = draw(&mut app);
    click(&mut app, find(&frame, "? Help")?);
    assert!(app.show_help);
    click(&mut app, find(&frame, "worker")?);
    assert!(app.show_help);
    assert_eq!(app.state.selected_service, 0);
    press_key(&mut app, KeyCode::Esc);

    // Quitting from a stray click would stop every service, so its hint does nothing.
    let frame = draw(&mut app);
    click(&mut app, find(&frame, "q Quit")?);
    assert!(app.running);

    // The arrows name two keys each, so a click on them is ambiguous and does nothing.
    let selected = app.state.selected_service;
    click(&mut app, find(&frame, "↑/↓ Move")?);
    assert_eq!(app.state.selected_service, selected);
    Ok(())
}

#[tokio::test]
async fn m_toggles_capture_and_the_footer_says_so() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    assert!(row_text(&draw(&mut app), HEIGHT - 1).contains("m Mouse:ON"));

    press_key(&mut app, KeyCode::Char('m'));
    assert!(!app.mouse.capture);
    assert_eq!(app.effects, vec![Effect::MouseCapture(false)]);
    let frame = draw(&mut app);
    let footer = (0..HEIGHT)
        .map(|y| row_text(&frame, y))
        .find(|row| row.contains("Mouse:"))
        .unwrap_or_default();
    assert!(footer.contains("m Mouse:OFF"), "{footer}");

    // Without capture, events the terminal sent before it stopped are ignored.
    click(&mut app, find(&frame, "worker")?);
    assert_eq!(app.state.selected_service, 0);

    press_key(&mut app, KeyCode::Char('m'));
    assert!(app.mouse.capture);
    assert_eq!(
        app.effects,
        vec![Effect::MouseCapture(false), Effect::MouseCapture(true)]
    );
    Ok(())
}

#[tokio::test]
async fn ctrl_z_asks_the_event_loop_to_suspend() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    app.handle_key_press(KeyEvent {
        code: KeyCode::Char('z'),
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    });
    assert_eq!(app.effects, vec![Effect::Suspend]);
    assert!(app.running);
    Ok(())
}

#[tokio::test]
async fn a_selection_in_one_service_is_dropped_when_another_is_selected() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_logs(&mut app, numbered(5))?;
    let frame = draw(&mut app);
    let line = find(&frame, "line 1 of")?;
    drag(&mut app, line, &[Position::new(line.x + 3, line.y)]);
    assert!(app.mouse.selection.is_some());

    app.focus = Focus::Services;
    press_key(&mut app, KeyCode::Down);
    draw(&mut app);
    assert!(app.mouse.selection.is_none());
    Ok(())
}

#[tokio::test]
async fn padding_after_the_text_is_neither_highlighted_nor_copied() -> eyre::Result<()> {
    // Terminal output pads its rows with spaces to the width of the service's terminal.
    let mut app = app(SERVICES)?;
    set_logs(
        &mut app,
        [
            format!("first{}", " ".repeat(60)),
            format!("second{}", " ".repeat(60)),
        ],
    )?;
    let frame = draw(&mut app);
    let first = find(&frame, "first")?;
    let second = find(&frame, "second")?;
    drag(&mut app, first, &[Position::new(second.x + 70, second.y)]);
    assert_eq!(copied(&mut app), vec!["first\nsecond".to_string()]);

    let frame = draw(&mut app);
    let reversed = |x: u16, y: u16| {
        frame
            .cell((x, y))
            .is_some_and(|cell| cell.modifier.contains(Modifier::REVERSED))
    };
    assert!(reversed(first.x + 4, first.y));
    assert!(!reversed(first.x + 5, first.y));
    Ok(())
}
