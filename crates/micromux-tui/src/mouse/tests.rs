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

use super::MouseState;
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
        // Sequence numbers start at 1, apart from the line indices.
        .map(|(index, line)| (u64::try_from(index).unwrap_or(u64::MAX) + 1, line))
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

/// Sends a mouse event; each turn of the wheel counts as a notch of its own.
fn mouse(app: &mut App, kind: MouseEventKind, position: Position) {
    if matches!(kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) {
        app.mouse.last_wheel = None;
    }
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

/// Copies the selection with `y`, as a user does, and returns what was copied.
fn yank(app: &mut App) -> Vec<String> {
    press_key(app, KeyCode::Char('y'));
    copied(app)
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
        yank(&mut app),
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
    assert_eq!(yank(&mut app), vec!["line ".to_string()]);

    // Esc drops the selection, and a plain click copies nothing.
    press_key(&mut app, KeyCode::Esc);
    assert!(app.mouse.selection.is_none());
    click(&mut app, Position::new(start.x + 1, start.y));
    assert_eq!(yank(&mut app), Vec::<String>::new());
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
    app.release(Position::new(0, 0));
    assert_eq!(yank(&mut app), vec!["line".to_string()]);

    // A third selects the whole line, and the rows of a wrapped line join without a break.
    for offset in 0..3 {
        app.press(
            Position::new(wrapped.x + 1, wrapped.y),
            now + Duration::from_millis(1000 + 100 * offset),
        );
    }
    app.release(Position::new(0, 0));
    assert_eq!(yank(&mut app), vec![long]);

    // A press that comes too late starts over.
    app.press(Position::new(short.x, short.y), now);
    app.press(
        Position::new(short.x, short.y),
        now + Duration::from_secs(1),
    );
    app.release(Position::new(0, 0));
    assert_eq!(yank(&mut app), Vec::<String>::new());
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
    let copied = yank(&mut app);
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
    assert_eq!(yank(&mut app), vec!["State".to_string()]);
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

#[cfg(unix)]
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
    assert_eq!(yank(&mut app), vec!["first\nsecond".to_string()]);

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

#[tokio::test]
async fn a_selection_stays_on_its_text_while_a_full_log_drops_its_oldest_lines() -> eyre::Result<()>
{
    let mut app = app(SERVICES)?;
    set_logs(&mut app, numbered(20))?;
    let frame = draw(&mut app);
    let line = find(&frame, "line 10 of")?;
    // Double-click selects "10" on line 10, and the release copies it.
    let now = Instant::now();
    let on_number = Position::new(line.x + 5, line.y);
    app.press(on_number, now);
    app.press(on_number, now + Duration::from_millis(50));
    app.release(on_number);
    assert_eq!(yank(&mut app), vec!["10".to_string()]);

    // The log reaches its cap: five new lines arrive and the five oldest go.
    let service = app
        .state
        .current_service_mut()
        .ok_or_else(|| eyre::eyre!("no service is selected"))?;
    for seq in 21..26u64 {
        service.cached_lines.pop_front();
        service
            .cached_lines
            .push_back((seq, format!("line {seq} of the log")));
    }
    service.text_dirty = true;
    let frame = draw(&mut app);

    // The highlight moved up with its line, and copying it again still gives that line's text.
    let moved = find(&frame, "line 10 of")?;
    assert_eq!(moved.y + 5, line.y);
    assert!(
        frame
            .cell((moved.x + 5, moved.y))
            .is_some_and(|cell| cell.modifier.contains(Modifier::REVERSED))
    );
    assert_eq!(app.selected_text().as_deref(), Some("10"));
    Ok(())
}

#[tokio::test]
async fn changing_what_the_log_lines_say_drops_the_selection() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_logs(&mut app, numbered(5))?;
    let frame = draw(&mut app);
    let line = find(&frame, "line 1 of")?;
    drag(&mut app, line, &[Position::new(line.x + 3, line.y)]);
    assert!(app.mouse.selection.is_some());

    // Timestamps change every structured line, so the selected cells no longer hold its text.
    press_key(&mut app, KeyCode::Char('T'));
    draw(&mut app);
    assert!(app.mouse.selection.is_none());
    Ok(())
}

#[tokio::test]
async fn an_unwrapped_line_is_copied_whole_past_the_panes_edge() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    let long = format!("start {} end", "x".repeat(150));
    set_logs(&mut app, ["short".to_string(), long.clone()])?;
    let frame = draw(&mut app);
    let row = find(&frame, "start")?;
    let now = Instant::now();

    // A triple click takes the whole line, though the pane shows only its start.
    for offset in 0..3 {
        app.press(row, now + Duration::from_millis(50 * offset));
    }
    app.release(row);
    assert_eq!(yank(&mut app), vec![long.clone()]);

    // So does a drag that leaves the pane on the right.
    let short = find(&frame, "short")?;
    drag(
        &mut app,
        Position::new(short.x + 2, short.y),
        &[Position::new(WIDTH - 1, row.y)],
    );
    assert_eq!(yank(&mut app), vec![format!("ort\n{long}")]);
    Ok(())
}

#[tokio::test]
async fn a_second_click_in_a_scrolled_list_picks_the_same_service() -> eyre::Result<()> {
    let mut services = String::new();
    for index in 0..40 {
        writeln!(
            services,
            "  svc{index:02}:\n    command: [\"sleep\", \"30\"]"
        )?;
    }
    let mut app = app(&format!("version: 1\nservices:\n{services}"))?;
    app.state.selected_service = 39;
    let frame = draw(&mut app);
    let target = find(&frame, "svc30")?;

    // The list does not jump when the selection moves up to a row it already shows.
    click(&mut app, target);
    draw(&mut app);
    click(&mut app, target);
    assert_eq!(
        app.state
            .current_service()
            .map(|service| service.snapshot.id.as_str()),
        Some("svc30")
    );
    Ok(())
}

#[tokio::test]
async fn in_pty_input_mode_the_mouse_never_changes_the_service_typed_into() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    press_key(&mut app, KeyCode::Char('a'));
    assert!(app.pty_input_mode);
    let frame = draw(&mut app);
    let web = find(&frame, "web")?;

    mouse(&mut app, MouseEventKind::ScrollDown, web);
    click(&mut app, web);
    assert_eq!(app.state.selected_service, 0);
    Ok(())
}

#[tokio::test]
async fn a_hint_acts_only_when_the_button_comes_up_on_it() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    let frame = draw(&mut app);
    let health = find(&frame, "H Health")?;

    // Pressing does nothing yet, and releasing elsewhere cancels the click.
    mouse(&mut app, MouseEventKind::Down(MouseButton::Left), health);
    assert!(!app.show_healthcheck_pane);
    mouse(
        &mut app,
        MouseEventKind::Drag(MouseButton::Left),
        Position::new(1, HEIGHT / 2),
    );
    mouse(
        &mut app,
        MouseEventKind::Up(MouseButton::Left),
        Position::new(1, HEIGHT / 2),
    );
    assert!(!app.show_healthcheck_pane);

    // Releasing on the hint it went down on runs its key.
    mouse(&mut app, MouseEventKind::Down(MouseButton::Left), health);
    mouse(&mut app, MouseEventKind::Up(MouseButton::Left), health);
    assert!(app.show_healthcheck_pane);
    Ok(())
}

#[tokio::test]
async fn a_blank_last_line_keeps_the_selection_on_its_text() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    // A burst of output ending with a blank line, as many services print.
    set_logs(&mut app, numbered(10).chain([String::new()]))?;
    let frame = draw(&mut app);
    let line = find(&frame, "line 5 of")?;
    drag(&mut app, line, &[Position::new(line.x + 5, line.y)]);
    assert_eq!(yank(&mut app), vec!["line 5".to_string()]);

    // Another line arrives, and later a blank one again, each pushing the text up.
    for text in ["next", ""] {
        let service = app
            .state
            .current_service_mut()
            .ok_or_else(|| eyre::eyre!("no service is selected"))?;
        let seq = service.cached_lines.back().map_or(1, |(seq, _)| seq + 1);
        service.cached_lines.pop_front();
        service.cached_lines.push_back((seq, text.to_string()));
        service.text_dirty = true;
        draw(&mut app);
        assert_eq!(
            app.selected_text().as_deref(),
            Some("line 5"),
            "after {text:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_taller_pane_scrolls_the_services_list_back_up() -> eyre::Result<()> {
    let mut services = String::new();
    for index in 0..40 {
        writeln!(
            services,
            "  svc{index:02}:\n    command: [\"sleep\", \"30\"]"
        )?;
    }
    let mut app = app(&format!("version: 1\nservices:\n{services}"))?;
    app.state.selected_service = 39;
    draw(&mut app);
    assert!(app.first_service > 0);

    // At a height that fits every service, the list starts at the first one again.
    let area = Rect::new(0, 0, WIDTH, 60);
    let mut frame = Buffer::empty(area);
    (&mut app).render(area, &mut frame);
    assert_eq!(app.first_service, 0);
    Ok(())
}

#[tokio::test]
async fn copying_a_very_long_unwrapped_line_lays_it_out_without_drawing_it() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    // As long as a log line can get, among the lines of a full log.
    let long = "y".repeat(16 * 1024);
    set_logs(
        &mut app,
        numbered(500).chain([long.clone()]).chain(numbered(10)),
    )?;
    let frame = draw(&mut app);
    let first = find(&frame, "yyyy")?;
    let last = find(&frame, "line 9 of")?;
    // Select from the start of the long line to the end of the last line.
    drag(&mut app, first, &[Position::new(WIDTH - 1, last.y)]);
    let copied = yank(&mut app);
    let text = copied.first().map(String::as_str).unwrap_or_default();
    assert!(text.contains(&long));
    assert!(
        text.ends_with("line 9 of the log"),
        "{}",
        &text[text.len() - 40..]
    );
    Ok(())
}

#[tokio::test]
async fn a_selection_stays_on_its_text_next_to_a_screen_snapshot_record() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    // A live screen snapshot is one record holding every row of the screen.
    set_logs(
        &mut app,
        ["snapshot row one\nsnapshot row two".to_string()]
            .into_iter()
            .chain(numbered(10)),
    )?;
    let frame = draw(&mut app);
    let line = find(&frame, "line 5 of")?;
    drag(&mut app, line, &[Position::new(line.x + 5, line.y)]);
    assert_eq!(yank(&mut app), vec!["line 5".to_string()]);

    // A row of the snapshot itself is found by its record and its row within it.
    let row = find(&frame, "snapshot row two")?;
    drag(&mut app, row, &[Position::new(row.x + 7, row.y)]);
    assert_eq!(yank(&mut app), vec!["snapshot".to_string()]);
    drag(&mut app, line, &[Position::new(line.x + 5, line.y)]);

    // The log drops its oldest record, the snapshot, and gains a line.
    let service = app
        .state
        .current_service_mut()
        .ok_or_else(|| eyre::eyre!("no service is selected"))?;
    let seq = service.cached_lines.back().map_or(1, |(seq, _)| seq + 1);
    service.cached_lines.pop_front();
    service.cached_lines.push_back((seq, "next".to_string()));
    service.text_dirty = true;
    draw(&mut app);
    assert_eq!(app.selected_text().as_deref(), Some("line 5"));
    Ok(())
}

#[tokio::test]
async fn a_double_click_takes_a_long_word_whole_past_the_panes_edge() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    let token = "z".repeat(16 * 1024);
    set_logs(&mut app, [format!("token {token} end")])?;
    let frame = draw(&mut app);
    let word = find(&frame, "zzzz")?;
    let now = Instant::now();
    app.press(word, now);
    app.press(word, now + Duration::from_millis(50));
    app.release(word);
    assert_eq!(yank(&mut app), vec![token]);
    Ok(())
}

fn key_with(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
    app.handle_key_press(KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    });
}

fn footer(app: &mut App) -> String {
    let frame = draw(app);
    (HEIGHT - 2..HEIGHT)
        .map(|y| row_text(&frame, y))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn selecting_only_highlights_and_copying_takes_an_explicit_key() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_logs(&mut app, numbered(5))?;
    let frame = draw(&mut app);
    let line = find(&frame, "line 2 of")?;
    let select = |app: &mut App| drag(app, line, &[Position::new(line.x + 5, line.y)]);

    // Ending the drag copies nothing and says nothing.
    select(&mut app);
    assert_eq!(copied(&mut app), Vec::<String>::new());
    assert_eq!(app.mouse.notice, None);

    // While text is selected, the footer offers the copy key in the mouse toggle's place.
    let shown = footer(&mut app);
    assert!(shown.contains("y Copy"), "{shown}");
    assert!(!shown.contains("Mouse:"), "{shown}");

    // `y`, Enter, and the terminals' copy chords each copy the same text, which stays selected.
    for (code, modifiers) in [
        (KeyCode::Char('y'), KeyModifiers::NONE),
        (KeyCode::Enter, KeyModifiers::NONE),
        (
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ),
        (KeyCode::Insert, KeyModifiers::CONTROL),
        (KeyCode::Char('c'), KeyModifiers::SUPER),
    ] {
        key_with(&mut app, code, modifiers);
        assert_eq!(copied(&mut app), vec!["line 2".to_string()], "{code:?}");
    }
    assert_eq!(app.mouse.notice.as_deref(), Some("copied 6 characters"));
    assert!(app.running);

    // Ctrl-C never copies, not even with text selected: it only says that `q` quits.
    key_with(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(copied(&mut app), Vec::<String>::new());
    assert!(app.quit_hint_at.is_some());
    assert!(app.running);
    app.quit_hint_at = None;

    // Clicking the copy hint copies too, without the press dropping the selection.
    let frame = draw(&mut app);
    click(&mut app, find(&frame, "y Copy")?);
    assert_eq!(copied(&mut app), vec!["line 2".to_string()]);

    // Without a selection, Enter and the copy key do nothing, and the footer shows the toggle.
    press_key(&mut app, KeyCode::Esc);
    press_key(&mut app, KeyCode::Enter);
    press_key(&mut app, KeyCode::Char('y'));
    assert_eq!(copied(&mut app), Vec::<String>::new());
    assert!(footer(&mut app).contains("m Mouse:ON"));
    Ok(())
}

#[tokio::test]
async fn copy_on_select_copies_when_the_drag_ends() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    app.mouse = MouseState::new(crate::MouseOptions {
        capture: true,
        copy_on_select: true,
    });
    set_logs(&mut app, numbered(5))?;
    let frame = draw(&mut app);
    let line = find(&frame, "line 2 of")?;
    drag(&mut app, line, &[Position::new(line.x + 5, line.y)]);
    assert_eq!(copied(&mut app), vec!["line 2".to_string()]);
    Ok(())
}

#[tokio::test]
async fn a_selection_holds_while_its_text_moves_and_copies_later() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    press_key(&mut app, KeyCode::Char('H'));
    set_logs(&mut app, numbered(40))?;
    let frame = draw(&mut app);
    let line = find(&frame, "line 30 of")?;
    drag(&mut app, line, &[Position::new(line.x + 6, line.y)]);

    // Lines arrive and the oldest go, the pane scrolls both ways and redraws many times.
    for round in 0..5u64 {
        let service = app
            .state
            .current_service_mut()
            .ok_or_else(|| eyre::eyre!("no service is selected"))?;
        let seq = service.cached_lines.back().map_or(1, |(seq, _)| seq + 1);
        service.cached_lines.pop_front();
        service
            .cached_lines
            .push_back((seq, format!("arrived in round {round}")));
        service.text_dirty = true;
        draw(&mut app);
    }
    let logs = find(&draw(&mut app), "Logs")?;
    mouse(
        &mut app,
        MouseEventKind::ScrollUp,
        Position::new(logs.x + 2, logs.y + 2),
    );
    draw(&mut app);
    mouse(
        &mut app,
        MouseEventKind::ScrollDown,
        Position::new(logs.x + 2, logs.y + 2),
    );
    for _ in 0..3 {
        draw(&mut app);
    }

    // A healthcheck refresh and a terminal resize leave the unwrapped log's selection alone.
    if let Some(service) = app.state.current_service_mut() {
        service.healthcheck_dirty = true;
    }
    draw(&mut app);
    let area = Rect::new(0, 0, WIDTH - 10, HEIGHT);
    let mut resized = Buffer::empty(area);
    (&mut app).render(area, &mut resized);
    draw(&mut app);

    // Copying now gives what was selected at the start.
    assert_eq!(yank(&mut app), vec!["line 30".to_string()]);
    Ok(())
}

#[tokio::test]
async fn a_selection_in_a_popup_holds_while_the_popup_redraws() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    press_key(&mut app, KeyCode::Char('i'));
    let frame = draw(&mut app);
    let state = find(&frame, "State")?;
    drag(&mut app, state, &[Position::new(state.x + 4, state.y)]);
    for _ in 0..3 {
        draw(&mut app);
    }
    assert_eq!(yank(&mut app), vec!["State".to_string()]);
    Ok(())
}

#[tokio::test]
async fn a_wrapped_selection_is_dropped_when_a_resize_rewraps_it() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_logs(&mut app, [format!("start {} end", "x".repeat(150))])?;
    press_key(&mut app, KeyCode::Char('w'));
    let frame = draw(&mut app);
    let start = find(&frame, "start")?;
    drag(&mut app, start, &[Position::new(start.x + 4, start.y)]);
    assert!(app.has_selection());

    let area = Rect::new(0, 0, WIDTH - 10, HEIGHT);
    let mut resized = Buffer::empty(area);
    (&mut app).render(area, &mut resized);
    assert!(!app.has_selection());
    Ok(())
}

#[tokio::test]
async fn one_notch_is_one_step_however_many_events_the_terminal_sends() -> eyre::Result<()> {
    let mut services = String::new();
    for index in 0..20 {
        writeln!(
            services,
            "  svc{index:02}:\n    command: [\"sleep\", \"30\"]"
        )?;
    }
    let mut app = app(&format!("version: 1\nservices:\n{services}"))?;
    let frame = draw(&mut app);
    let list = find(&frame, "svc05")?;
    let now = Instant::now();

    // A notch reported as a burst of three events moves one service.
    for _ in 0..3 {
        app.wheel(list, true, now);
    }
    assert_eq!(app.state.selected_service, 1);

    // The notches of a fast spin, 20 ms apart, each move one service.
    for notch in 1..=10u64 {
        app.wheel(list, true, now + Duration::from_millis(20 * notch));
    }
    assert_eq!(app.state.selected_service, 11);

    // How late the TUI gets to them does not matter, only when they arrived.
    let start = now + Duration::from_secs(1);
    for notch in 0..5u64 {
        app.wheel(list, true, start + Duration::from_millis(20 * notch));
    }
    assert_eq!(app.state.selected_service, 16);

    // The list never wraps around.
    for notch in 1..=30u64 {
        app.wheel(
            list,
            true,
            now + Duration::from_secs(2) + Duration::from_millis(100 * notch),
        );
    }
    assert_eq!(app.state.selected_service, 19);
    let later = now + Duration::from_secs(10);
    app.wheel(list, false, later);
    assert_eq!(app.state.selected_service, 18);

    // The list scrolls along, keeping the selection on screen.
    let frame = draw(&mut app);
    assert!(find(&frame, "svc18").is_ok());
    Ok(())
}

#[tokio::test]
async fn the_wheel_leaves_a_pane_with_nothing_to_scroll_following() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    // Like a full-screen program's live screen, the text fits the pane.
    set_logs(&mut app, numbered(3))?;
    let frame = draw(&mut app);
    let logs = find(&frame, "Logs")?;
    let inside = Position::new(logs.x + 2, logs.y + 2);
    for down in [false, true, false, false, true] {
        let kind = if down {
            MouseEventKind::ScrollDown
        } else {
            MouseEventKind::ScrollUp
        };
        mouse(&mut app, kind, inside);
        assert!(app.log_view.follow_tail);
        assert!(row_text(&draw(&mut app), 0).contains("Tail:ON"));
    }
    Ok(())
}

#[tokio::test]
async fn the_focused_pane_has_a_thick_accented_frame() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    press_key(&mut app, KeyCode::Char('H'));
    // Each pane's top-left corner, found by its name drawn right after it.
    let corners = |app: &mut App| -> eyre::Result<Vec<(String, String)>> {
        let frame = draw(app);
        ["Services", "Logs", "Healthcheck"]
            .into_iter()
            .map(|name| {
                let at = find(&frame, name)?;
                let corner = frame
                    .cell((at.x - 1, at.y))
                    .ok_or_else(|| eyre::eyre!("no corner before {name}"))?;
                Ok((name.to_string(), corner.symbol().to_string()))
            })
            .collect()
    };
    let thick = |corners: &[(String, String)]| {
        corners
            .iter()
            .filter(|(_, corner)| corner == "┏")
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
    };

    // Exactly the focused pane is drawn thick, which tells it apart without any color.
    assert_eq!(thick(&corners(&mut app)?), vec!["Services".to_string()]);
    press_key(&mut app, KeyCode::Tab);
    assert_eq!(thick(&corners(&mut app)?), vec!["Logs".to_string()]);
    press_key(&mut app, KeyCode::Tab);
    assert_eq!(thick(&corners(&mut app)?), vec!["Healthcheck".to_string()]);

    // A click moves the frame as Tab does, and the focused frame takes the accent color.
    let frame = draw(&mut app);
    click(&mut app, find(&frame, "web")?);
    assert_eq!(thick(&corners(&mut app)?), vec!["Services".to_string()]);
    let frame = draw(&mut app);
    let services = find(&frame, "Services")?;
    let logs = find(&frame, "Logs")?;
    let color = |at: Position| frame.cell((at.x - 1, at.y)).map(|cell| cell.fg);
    assert_eq!(color(services), Some(crate::render::FOCUS_COLOR));
    assert_ne!(color(logs), Some(crate::render::FOCUS_COLOR));
    Ok(())
}

#[tokio::test]
async fn the_copy_hint_copies_a_selection_in_a_popup_without_closing_it() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    press_key(&mut app, KeyCode::Char('?'));
    let frame = draw(&mut app);
    let text = find(&frame, "Log level threshold")?;
    drag(&mut app, text, &[Position::new(text.x + 2, text.y)]);
    let frame = draw(&mut app);

    click(&mut app, find(&frame, "y Copy")?);
    assert_eq!(copied(&mut app), vec!["Log".to_string()]);
    assert!(app.show_help);

    // The level picker has no use for `y`, so its footer offers no copy.
    press_key(&mut app, KeyCode::Esc);
    press_key(&mut app, KeyCode::Char('L'));
    assert!(!footer(&mut app).contains("y Copy"));
    Ok(())
}

#[tokio::test]
async fn a_copy_chord_copies_a_selection_in_a_popup() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    press_key(&mut app, KeyCode::Char('i'));
    let frame = draw(&mut app);
    let text = find(&frame, "State")?;
    drag(&mut app, text, &[Position::new(text.x + 4, text.y)]);
    key_with(
        &mut app,
        KeyCode::Char('c'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
    assert_eq!(copied(&mut app), vec!["State".to_string()]);
    assert!(app.info.is_some());
    Ok(())
}

fn right_click(app: &mut App, position: Position) {
    mouse(app, MouseEventKind::Down(MouseButton::Right), position);
    mouse(app, MouseEventKind::Up(MouseButton::Right), position);
}

/// A right-click copies the selection as `y` does, wherever it lands, and changes nothing else:
/// not the selection, the focus or the selected service.
#[tokio::test]
async fn a_right_click_copies_the_selection_and_changes_nothing_else() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_logs(&mut app, numbered(5))?;
    let frame = draw(&mut app);

    // Without a selection it does nothing.
    right_click(&mut app, find(&frame, "worker")?);
    assert_eq!(copied(&mut app), Vec::<String>::new());
    assert_eq!(app.state.selected_service, 0);

    let line = find(&frame, "line 2 of")?;
    drag(&mut app, line, &[Position::new(line.x + 5, line.y)]);
    app.focus = Focus::Services;
    let selection = app.mouse.selection.clone();

    // Over another service, over other log text and over the selection itself.
    for position in [
        find(&frame, "worker")?,
        find(&frame, "line 4 of")?,
        Position::new(line.x + 2, line.y),
    ] {
        right_click(&mut app, position);
        assert_eq!(copied(&mut app), vec!["line 2".to_string()], "{position:?}");
        assert_eq!(app.mouse.selection, selection);
        assert_eq!(app.focus, Focus::Services);
        assert_eq!(app.state.selected_service, 0);
    }
    assert_eq!(app.mouse.notice.as_deref(), Some("copied 6 characters"));

    // The release copies, so a press alone does nothing yet.
    mouse(&mut app, MouseEventKind::Down(MouseButton::Right), line);
    assert_eq!(copied(&mut app), Vec::<String>::new());
    Ok(())
}

#[tokio::test]
async fn a_right_click_copies_in_the_healthcheck_pane_and_popups() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    press_key(&mut app, KeyCode::Char('H'));
    let frame = draw(&mut app);
    let text = find(&frame, "no healthcheck")?;
    drag(&mut app, text, &[Position::new(text.x + 1, text.y)]);
    right_click(&mut app, Position::new(0, 0));
    assert_eq!(copied(&mut app), vec!["no".to_string()]);

    for (key, word) in [('?', "Select"), ('i', "State"), ('u', "reports")] {
        press_key(&mut app, KeyCode::Esc);
        press_key(&mut app, KeyCode::Char(key));
        let frame = draw(&mut app);
        let text = find(&frame, word)?;
        let end = u16::try_from(word.len())? - 1;
        drag(&mut app, text, &[Position::new(text.x + end, text.y)]);
        // Outside the popup, where a left click would close it.
        right_click(&mut app, Position::new(0, 0));
        assert_eq!(copied(&mut app), vec![word.to_string()], "{key}");
        assert!(app.show_help || app.info.is_some(), "{key}");
    }
    Ok(())
}

/// In PTY input mode the keys belong to the service, so with text selected neither the copy
/// shortcuts nor a right-click copy, and Ctrl-C neither prompts nor quits.
#[tokio::test]
async fn nothing_copies_or_quits_in_pty_input_mode() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_logs(&mut app, numbered(5))?;
    let frame = draw(&mut app);
    let line = find(&frame, "line 2 of")?;
    drag(&mut app, line, &[Position::new(line.x + 5, line.y)]);
    assert!(app.has_selection());
    press_key(&mut app, KeyCode::Char('a'));
    assert!(app.pty_input_mode);
    for _ in 0..2 {
        for (code, modifiers) in [
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
            (
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            (KeyCode::Insert, KeyModifiers::CONTROL),
            (KeyCode::Char('c'), KeyModifiers::SUPER),
            (KeyCode::Char('y'), KeyModifiers::NONE),
            (KeyCode::Enter, KeyModifiers::NONE),
        ] {
            key_with(&mut app, code, modifiers);
        }
        right_click(&mut app, line);
    }
    assert_eq!(copied(&mut app), Vec::<String>::new());
    assert!(app.has_selection());
    assert_eq!(app.quit_hint_at, None);
    assert!(app.running);
    Ok(())
}

/// A drag that starts below a short log's last line starts at that line's end, so lines that
/// arrive later never join the selection.
#[tokio::test]
async fn a_selection_started_below_the_text_never_takes_in_new_lines() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_logs(&mut app, numbered(3))?;
    let frame = draw(&mut app);
    let first = find(&frame, "line 0 of")?;
    let below = Position::new(first.x + 10, first.y + 10);
    drag(&mut app, below, &[first]);
    let selected = vec!["line 0 of the log\nline 1 of the log\nline 2 of the log".to_string()];
    assert_eq!(yank(&mut app), selected);

    set_logs(&mut app, numbered(8))?;
    assert_eq!(yank(&mut app), selected);

    // A drag down into the space below the text takes the last line whole, and one only across
    // that space takes nothing.
    set_logs(&mut app, numbered(3))?;
    let frame = draw(&mut app);
    let first = find(&frame, "line 0 of")?;
    app.mouse.last_press = None;
    drag(&mut app, first, &[Position::new(first.x + 2, first.y + 10)]);
    assert_eq!(yank(&mut app), selected);
    app.mouse.last_press = None;
    drag(
        &mut app,
        Position::new(first.x + 2, first.y + 10),
        &[Position::new(first.x + 30, first.y + 12)],
    );
    assert_eq!(yank(&mut app), Vec::<String>::new());

    // A triple click below the text takes the last line, and no later one.
    let now = Instant::now();
    set_logs(&mut app, numbered(3))?;
    let frame = draw(&mut app);
    let first = find(&frame, "line 0 of")?;
    for offset in 0..3 {
        app.press(
            Position::new(first.x + 2, first.y + 10),
            now + Duration::from_millis(100 * offset),
        );
    }
    app.release(Position::new(0, 0));
    set_logs(&mut app, numbered(8))?;
    assert_eq!(yank(&mut app), vec!["line 2 of the log".to_string()]);
    Ok(())
}

/// A one-character word can be selected by a double click or a drag that ends where it
/// started, while a plain click selects nothing.
#[tokio::test]
async fn a_single_cell_can_be_selected() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_logs(&mut app, ["exit 1 done".to_string()])?;
    let frame = draw(&mut app);
    let one = find(&frame, " 1 ")?;
    let one = Position::new(one.x + 1, one.y);
    let now = Instant::now();
    app.press(one, now);
    app.press(one, now + Duration::from_millis(100));
    app.release(one);
    assert_eq!(yank(&mut app), vec!["1".to_string()]);

    // A drag away and back keeps that one cell; its press starts no series with the earlier
    // ones.
    press_key(&mut app, KeyCode::Esc);
    app.mouse.last_press = None;
    drag(&mut app, one, &[Position::new(one.x + 3, one.y), one]);
    assert_eq!(yank(&mut app), vec!["1".to_string()]);

    // A plain click only places a caret.
    press_key(&mut app, KeyCode::Esc);
    app.mouse.last_press = None;
    click(&mut app, Position::new(one.x + 5, one.y));
    assert_eq!(yank(&mut app), Vec::<String>::new());
    assert!(!footer(&mut app).contains("y Copy"));
    Ok(())
}

/// Ctrl+] leaves PTY input mode as every terminal sends it, the byte 0x1d that crossterm reads as
/// Ctrl+5, and as the kitty protocol reports it; so does Alt+Esc where it arrives apart, while a
/// plain Esc goes to the service.
#[tokio::test]
async fn ctrl_close_bracket_leaves_pty_input_mode() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    for (code, modifiers) in [
        (KeyCode::Char('5'), KeyModifiers::CONTROL),
        (KeyCode::Char(']'), KeyModifiers::CONTROL),
        (KeyCode::Esc, KeyModifiers::ALT),
    ] {
        press_key(&mut app, KeyCode::Char('a'));
        assert!(app.pty_input_mode);
        press_key(&mut app, KeyCode::Esc);
        assert!(app.pty_input_mode);
        key_with(&mut app, code, modifiers);
        assert!(!app.pty_input_mode, "{code:?} {modifiers:?}");
    }

    // Its footer hint says so, and a click on it leaves the mode too.
    press_key(&mut app, KeyCode::Char('a'));
    let frame = draw(&mut app);
    click(&mut app, find(&frame, "Ctrl+] Exit")?);
    assert!(!app.pty_input_mode);
    Ok(())
}
