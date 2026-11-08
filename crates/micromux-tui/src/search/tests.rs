use std::path::Path;
use std::time::Instant;

use color_eyre::eyre;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use indoc::indoc;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Widget as _;
use similar_asserts::assert_eq;

use super::{Match, Query, find_matches};
use crate::state::CachedLine;
use crate::{App, LocalSource, SessionSource, ViewOptions};

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

/// The byte ranges `query` matches in `haystack`, as the text they cover.
fn found<'a>(query: &str, haystack: &'a str) -> eyre::Result<Vec<&'a str>> {
    let query = Query::new(query).ok_or_else(|| eyre::eyre!("empty query"))?;
    query
        .find_in(haystack)
        .into_iter()
        .map(|bytes| {
            haystack
                .get(bytes.clone())
                .ok_or_else(|| eyre::eyre!("{bytes:?} is not a char range of {haystack:?}"))
        })
        .collect()
}

/// An app over `yaml`'s services that has drawn one frame.
fn app(yaml: &str) -> eyre::Result<App> {
    let (app, _) = app_and_runner(yaml)?;
    Ok(app)
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

/// Gives the selected service the records `(seq, line)` as its log, as if it had printed them,
/// and draws a frame.
fn set_records(
    app: &mut App,
    records: impl IntoIterator<Item = (u64, String)>,
) -> eyre::Result<()> {
    let service = app
        .state
        .current_service_mut()
        .ok_or_else(|| eyre::eyre!("no service is selected"))?;
    // The first frame read the model's log already, so these lines are not overwritten.
    service.logs_dirty = false;
    service.cached_lines = records
        .into_iter()
        .map(|(seq, text)| CachedLine {
            seq,
            timestamp_unix_ms: 0,
            text,
        })
        .collect();
    service.text_dirty = true;
    draw(app);
    Ok(())
}

/// A log of `lines` numbered records, with sequence numbers from 1, whose lines at `needles`
/// mention a needle.
fn log_with_needles(lines: u64, needles: &[u64]) -> Vec<(u64, String)> {
    log_range(0..lines, needles)
}

/// The records of [`log_with_needles`] numbered `indices`, as a log that dropped the ones before.
fn log_range(indices: std::ops::Range<u64>, needles: &[u64]) -> Vec<(u64, String)> {
    indices
        .map(|index| {
            let line = if needles.contains(&index) {
                format!("needle at {index:03}")
            } else {
                format!("line {index:03}")
            };
            (index + 1, line)
        })
        .collect()
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

fn press(app: &mut App, code: KeyCode) {
    let modifiers = match code {
        KeyCode::Char(ch) if ch.is_uppercase() => KeyModifiers::SHIFT,
        _ => KeyModifiers::NONE,
    };
    app.handle_key_press(KeyEvent {
        code,
        modifiers,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    });
}

fn type_text(app: &mut App, text: &str) {
    for ch in text.chars() {
        press(app, KeyCode::Char(ch));
    }
}

fn search(app: &mut App, query: &str) {
    press(app, KeyCode::Char('/'));
    type_text(app, query);
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

/// The style of the cell where `needle` starts on screen.
fn style_at(buffer: &Buffer, needle: &str) -> eyre::Result<(Color, Modifier)> {
    let position = find(buffer, needle)?;
    let cell = buffer
        .cell(position)
        .ok_or_else(|| eyre::eyre!("no cell at {position:?}"))?;
    Ok((cell.fg, cell.modifier))
}

/// Whether `needle` is drawn as the current match, in bold reverse video of the focus accent.
fn is_current(buffer: &Buffer, needle: &str) -> eyre::Result<bool> {
    let (fg, modifier) = style_at(buffer, needle)?;
    Ok(fg == crate::render::FOCUS_COLOR && modifier.contains(Modifier::REVERSED | Modifier::BOLD))
}

/// Whether `needle` is drawn as another match, in reverse video of yellow.
fn is_other_match(buffer: &Buffer, needle: &str) -> eyre::Result<bool> {
    let (fg, modifier) = style_at(buffer, needle)?;
    Ok(fg == Color::Yellow
        && modifier.contains(Modifier::REVERSED)
        && !modifier.contains(Modifier::BOLD))
}

#[test]
fn smartcase_ignores_case_until_the_query_has_an_uppercase_letter() -> eyre::Result<()> {
    let haystack = "Error: error ERROR";

    // Lowercase queries ignore case.
    assert_eq!(found("error", haystack)?, vec!["Error", "error", "ERROR"]);

    // An uppercase letter makes the query match case exactly.
    assert_eq!(found("Error", haystack)?, vec!["Error"]);
    assert_eq!(found("ERROR", haystack)?, vec!["ERROR"]);

    // An empty query searches for nothing.
    assert_eq!(Query::new(""), None);
    Ok(())
}

#[test]
fn matches_cover_whole_characters_and_never_overlap() -> eyre::Result<()> {
    // Matches go left to right and the next one starts after the last ends.
    assert_eq!(found("aa", "aaaaa")?, vec!["aa", "aa"]);

    // Multi-byte and wide characters match as whole characters, with or without case.
    assert_eq!(found("日本", "日本語 と 日本")?, vec!["日本", "日本"]);
    assert_eq!(found("ärger", "Kein ÄRGER, ärger")?, vec!["ÄRGER", "ärger"]);

    // A character whose lowercase is shorter in bytes, the Kelvin sign, still matches whole.
    assert_eq!(found("5k", "5\u{212a} and 5k")?, vec!["5\u{212a}", "5k"]);

    // A character that lowercases to several characters matches only itself.
    assert_eq!(found("i̇", "İ")?, Vec::<&str>::new());
    assert_eq!(found("ab", "İab")?, vec!["ab"]);
    Ok(())
}

#[test]
fn matches_widen_to_whole_graphemes() -> eyre::Result<()> {
    // A decomposed accent and an emoji presentation selector stay with their base character.
    assert_eq!(found("e", "cafe\u{301} e")?, vec!["e\u{301}", "e"]);
    assert_eq!(found("❤", "❤\u{fe0f} ok")?, vec!["❤\u{fe0f}"]);

    // A combining mark alone highlights the whole grapheme it belongs to.
    assert_eq!(found("\u{301}", "e\u{301}")?, vec!["e\u{301}"]);
    Ok(())
}

/// A match is placed on the wrapped row it is drawn on, except in a line too long to draw off
/// screen cheaply, which scrolls to its first row.
#[test]
fn matches_are_placed_on_their_wrapped_row_unless_the_line_is_huge() {
    let line_with_needle_after = |filler_words: usize| {
        let text = format!("{}needle", "filler ".repeat(filler_words));
        let start = text.len() - "needle".len();
        (Line::raw(text), start..start + "needle".len())
    };

    // Three filler words fill a row of twenty columns, so the tenth starts the fourth row and
    // the needle follows it there.
    let (line, bytes) = line_with_needle_after(10);
    assert_eq!(super::row_in_wrapped_line(&line, &bytes, 20), 3);

    // A line past the limit is not drawn, however deep the match.
    let (line, bytes) = line_with_needle_after(super::MAX_PLACED_LINE_BYTES / 7 + 1);
    assert_eq!(super::row_in_wrapped_line(&line, &bytes, 20), 0);
}

#[test]
fn matches_span_the_styled_runs_of_a_line() -> eyre::Result<()> {
    let text = Text::from(vec![
        Line::from(vec![Span::raw("an err"), Span::raw("or here")]),
        Line::raw("nothing"),
        Line::raw("error, error"),
    ]);
    let query = Query::new("error").ok_or_else(|| eyre::eyre!("empty query"))?;

    // A match may start in one span and end in the next; lines without one are skipped.
    assert_eq!(
        find_matches(&text, &query),
        vec![
            Match {
                line: 0,
                bytes: 3..8,
            },
            Match {
                line: 2,
                bytes: 0..5,
            },
            Match {
                line: 2,
                bytes: 7..12,
            },
        ]
    );
    Ok(())
}

/// Confirming a query moves to the newest match at or above the bottom of the view, and `n` and
/// `N` walk older and newer, wrapping around, scrolling each match into view.
#[tokio::test]
async fn n_and_shift_n_walk_the_matches_back_and_forth_in_time() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_records(&mut app, log_with_needles(100, &[10, 50, 90]))?;
    let viewport = usize::from(app.log_viewport_height());
    assert!(viewport < 40, "the matches must not all fit on screen");

    // Following the tail, the newest match is on screen already, so the view stays.
    search(&mut app, "needle");
    let frame = draw(&mut app);
    assert!(screen(&frame).contains("/needle 3/3"), "{}", screen(&frame));
    assert!(is_current(&frame, "needle at 090")?);
    assert!(!app.log_view.follow_tail);
    assert_eq!(app.log_view.scroll_offset, 100 - viewport);

    // `n` goes back to the older match, centering it.
    press(&mut app, KeyCode::Char('n'));
    let frame = draw(&mut app);
    assert!(screen(&frame).contains("/needle 2/3"));
    assert!(is_current(&frame, "needle at 050")?);
    assert_eq!(app.log_view.scroll_offset, 50 - viewport / 2);

    // Past the oldest match, `n` wraps to the newest.
    press(&mut app, KeyCode::Char('n'));
    assert!(screen(&draw(&mut app)).contains("/needle 1/3"));
    press(&mut app, KeyCode::Char('n'));
    assert!(screen(&draw(&mut app)).contains("/needle 3/3"));

    // `N` goes the other way, wrapping from the newest to the oldest.
    press(&mut app, KeyCode::Char('N'));
    let frame = draw(&mut app);
    assert!(screen(&frame).contains("/needle 1/3"));
    assert!(is_current(&frame, "needle at 010")?);
    assert_eq!(app.log_view.scroll_offset, 0);
    Ok(())
}

/// Every match on screen is highlighted, the current one apart from the others.
#[tokio::test]
async fn visible_matches_are_highlighted_and_the_current_one_stands_out() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_records(
        &mut app,
        [
            (1, "GET /a took 3ms".to_string()),
            (2, "GET /b took 4ms".to_string()),
            (3, "post /c".to_string()),
        ],
    )?;

    search(&mut app, "get");
    let frame = draw(&mut app);

    // Smartcase finds both uppercase requests; the newer is current.
    assert!(screen(&frame).contains("/get 2/2"));
    assert!(is_current(&frame, "GET /b")?);
    assert!(is_other_match(&frame, "GET /a")?);

    // Only the matched characters are highlighted.
    let (_, modifier) = style_at(&frame, " /a took")?;
    assert!(!modifier.contains(Modifier::REVERSED));
    let (_, modifier) = style_at(&frame, "post")?;
    assert!(!modifier.contains(Modifier::REVERSED));
    Ok(())
}

/// The prompt edits a query, takes every key, `q` included, and leaves the previous search as
/// it was when cancelled.
#[tokio::test]
async fn the_prompt_edits_confirms_and_cancels_a_query() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_records(&mut app, log_with_needles(5, &[1, 3]))?;

    // Typed keys, even quit, edit the query, which shows over the footer's last row.
    press(&mut app, KeyCode::Char('/'));
    type_text(&mut app, "neqx");
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    assert!(app.running);
    let frame = draw(&mut app);
    let last_row = row_text(&frame, HEIGHT - 1);
    assert!(last_row.starts_with(" /ne "), "{last_row:?}");
    assert!(last_row.contains("↵ search · Esc cancel"), "{last_row:?}");
    // The hints the prompt covers cannot be clicked.
    assert!(app.hits.hints.iter().all(|hit| hit.area.y != HEIGHT - 1));

    // Esc cancels without searching.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.search.prompt, None);
    assert!(!app.search.is_active());

    // A confirmed search survives a cancelled prompt.
    search(&mut app, "needle");
    press(&mut app, KeyCode::Char('/'));
    type_text(&mut app, "x");
    press(&mut app, KeyCode::Esc);
    assert!(screen(&draw(&mut app)).contains("/needle 2/2"));

    // Backspace on an empty prompt closes it, and so does Enter, keeping the search.
    press(&mut app, KeyCode::Char('/'));
    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.search.prompt, None);
    press(&mut app, KeyCode::Char('/'));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.search.prompt, None);
    assert!(screen(&draw(&mut app)).contains("/needle 2/2"));

    // A query without matches says so.
    search(&mut app, "Needle");
    assert!(screen(&draw(&mut app)).contains("/Needle no match"));
    Ok(())
}

/// `Esc` drops a mouse selection first and the search with its highlighting next, after which
/// `n` does nothing.
#[tokio::test]
async fn escape_clears_the_selection_before_the_search() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_records(&mut app, log_with_needles(5, &[1, 3]))?;
    search(&mut app, "needle");
    let frame = draw(&mut app);
    let from = find(&frame, "line 000")?;
    let to = find(&frame, "line 002")?;
    mouse(&mut app, MouseEventKind::Down(MouseButton::Left), from);
    mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), to);
    mouse(&mut app, MouseEventKind::Up(MouseButton::Left), to);
    assert!(app.has_selection());

    // The selection flips reverse video, so it shows over the matches it covers too.
    let frame = draw(&mut app);
    let (_, modifier) = style_at(&frame, "line 000")?;
    assert!(modifier.contains(Modifier::REVERSED));
    let (fg, modifier) = style_at(&frame, "needle at 001")?;
    assert_eq!(fg, Color::Yellow);
    assert!(!modifier.contains(Modifier::REVERSED));

    press(&mut app, KeyCode::Esc);
    assert!(!app.has_selection());
    assert!(app.search.is_active());

    press(&mut app, KeyCode::Esc);
    assert!(!app.search.is_active());
    let frame = draw(&mut app);
    assert!(!screen(&frame).contains("/needle"));
    let (_, modifier) = style_at(&frame, "needle at 003")?;
    assert!(!modifier.contains(Modifier::REVERSED));

    // Without a search, `n` leaves the view alone.
    let offset = app.log_view.scroll_offset;
    press(&mut app, KeyCode::Char('n'));
    assert_eq!(app.log_view.scroll_offset, offset);
    Ok(())
}

/// The current match stays on its text while new lines arrive and the oldest are dropped, and
/// the count follows the matches still in the log.
#[tokio::test]
async fn the_current_match_follows_its_text_as_the_log_moves_on() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_records(&mut app, log_with_needles(10, &[1, 5, 8]))?;
    search(&mut app, "needle");
    press(&mut app, KeyCode::Char('n'));
    let frame = draw(&mut app);
    assert!(screen(&frame).contains("/needle 2/3"));
    assert!(is_current(&frame, "needle at 005")?);

    // The oldest records, with the first match, are dropped and a new match arrives.
    let mut records = log_with_needles(12, &[1, 5, 8, 11]);
    records.drain(..3);
    set_records(&mut app, records)?;
    let frame = draw(&mut app);
    assert!(screen(&frame).contains("/needle 1/3"), "{}", screen(&frame));
    assert!(is_current(&frame, "needle at 005")?);
    assert!(is_other_match(&frame, "needle at 011")?);

    // The current match's record is dropped too, so the next newer match becomes current.
    let mut records = log_with_needles(12, &[1, 5, 8, 11]);
    records.drain(..7);
    set_records(&mut app, records)?;
    let frame = draw(&mut app);
    assert!(screen(&frame).contains("/needle 1/2"), "{}", screen(&frame));
    assert!(is_current(&frame, "needle at 008")?);
    Ok(())
}

/// Another service keeps the query and finds its own matches, with none current until `n`
/// moves to one.
#[tokio::test]
async fn switching_services_searches_the_new_log_for_the_same_query() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_records(&mut app, log_with_needles(5, &[1]))?;
    search(&mut app, "needle");
    assert!(screen(&draw(&mut app)).contains("/needle 1/1"));

    press(&mut app, KeyCode::Down);
    assert_eq!(app.state.selected_service, 1);
    // The first frame of the service reads its log from the model, which would replace records
    // given before it.
    draw(&mut app);
    set_records(&mut app, log_with_needles(5, &[2, 4]))?;
    let frame = draw(&mut app);
    assert!(
        screen(&frame).contains("/needle 2 matches"),
        "{}",
        screen(&frame)
    );
    assert!(is_other_match(&frame, "needle at 004")?);

    press(&mut app, KeyCode::Char('n'));
    let frame = draw(&mut app);
    assert!(screen(&frame).contains("/needle 2/2"));
    assert!(is_current(&frame, "needle at 004")?);
    Ok(())
}

/// In PTY input mode `/`, `n` and `N` reach the service instead of searching.
#[tokio::test]
async fn pty_input_mode_types_search_keys_into_the_service() -> eyre::Result<()> {
    let (mut app, _runner) = app_and_runner(SERVICES)?;
    set_records(&mut app, log_with_needles(5, &[1, 3]))?;
    search(&mut app, "needle");
    let offset = app.log_view.scroll_offset;

    // With the scheduler's queue full, typed input waits in the TUI, where it can be read.
    let terminal = app
        .input
        .clone()
        .ok_or_else(|| eyre::eyre!("the app has no PTY input"))?;
    while terminal
        .try_send(terminal.prepare_input("api".to_string(), vec![b'x'])?)
        .is_ok()
    {}

    app.pty_input_mode = true;
    press(&mut app, KeyCode::Char('/'));
    press(&mut app, KeyCode::Char('n'));
    press(&mut app, KeyCode::Char('N'));

    // The keys went to the selected service as typed, and the search did not move.
    let typed = app
        .pending_pty_input
        .iter()
        .inspect(|input| assert_eq!(input.service_id(), "api"))
        .flat_map(|input| input.data().to_vec())
        .collect::<Vec<u8>>();
    assert_eq!(typed, b"/nN".to_vec());
    assert_eq!(app.search.prompt, None);
    assert_eq!(app.log_view.scroll_offset, offset);
    assert!(screen(&draw(&mut app)).contains("/needle 2/2"));
    Ok(())
}

/// A click while the prompt is open only closes it, so neither a hint the prompt covers nor
/// one beside it acts or types its key into the query.
#[tokio::test]
async fn clicks_on_hints_only_close_the_prompt() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_records(&mut app, log_with_needles(5, &[1, 3]))?;
    let before = draw(&mut app);
    let mouse_hint = find(&before, "m Mouse")?;
    let help_hint = find(&before, "? Help")?;
    assert_eq!(
        mouse_hint.y,
        HEIGHT - 1,
        "the mouse hint must be on the covered row"
    );
    assert_ne!(
        help_hint.y,
        HEIGHT - 1,
        "the help hint must be on an uncovered row"
    );

    for hint in [mouse_hint, help_hint] {
        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "ne");
        draw(&mut app);
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), hint);
        mouse(&mut app, MouseEventKind::Up(MouseButton::Left), hint);

        // The prompt closed without searching, and the hint's key did nothing.
        assert_eq!(app.search.prompt, None);
        assert!(!app.search.is_active());
        assert!(app.mouse.capture);
        assert_eq!(app.effects, Vec::new());
        assert!(!app.show_help);
    }
    Ok(())
}

/// A paste adds its first line to the query, and the query stops growing at its limit while
/// the prompt shows its end.
#[tokio::test]
async fn pastes_extend_the_query_up_to_its_limit() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    press(&mut app, KeyCode::Char('/'));
    app.handle_crossterm_event(&crossterm::event::Event::Paste("nee\ndle".to_string()));
    assert_eq!(app.search.prompt.as_deref(), Some("nee"));

    let long = format!("{}end", "x".repeat(1_000));
    app.handle_crossterm_event(&crossterm::event::Event::Paste(long));
    type_text(&mut app, "y");
    let prompt = app.search.prompt.clone().unwrap_or_default();
    assert_eq!(prompt.chars().count(), super::MAX_QUERY_CHARS);
    assert!(prompt.starts_with("neexxx"));

    // The row shows the query's end, right up to the cursor.
    let last_row = row_text(&draw(&mut app), HEIGHT - 1);
    assert!(last_row.starts_with(" /xxx"), "{last_row:?}");
    assert_eq!(last_row.chars().count(), usize::from(WIDTH));
    Ok(())
}

/// Characters typed with `AltGr`, which Windows reports as Ctrl+Alt, and with Alt go into the
/// query; Ctrl chords do not.
#[tokio::test]
async fn the_prompt_takes_altgr_characters_but_not_ctrl_chords() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    press(&mut app, KeyCode::Char('/'));
    let altgr = KeyModifiers::CONTROL | KeyModifiers::ALT;
    for (ch, modifiers) in [
        ('@', altgr),
        ('{', altgr),
        ('\\', altgr),
        ('~', KeyModifiers::ALT),
        ('x', KeyModifiers::CONTROL),
        ('X', KeyModifiers::CONTROL | KeyModifiers::SHIFT),
    ] {
        app.handle_key_press(KeyEvent {
            code: KeyCode::Char(ch),
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
    }
    assert_eq!(app.search.prompt.as_deref(), Some("@{\\~"));
    Ok(())
}

/// A match of a long wrapped line is scrolled into view by the row it is on, not by the row its
/// line starts on.
#[tokio::test]
async fn a_match_deep_in_a_wrapped_line_is_scrolled_into_view() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    press(&mut app, KeyCode::Char('w'));
    let mut records = vec![(1, format!("{}needle here", "filler ".repeat(500)))];
    records.extend((2..40).map(|seq| (seq, format!("line {seq:03}"))));
    set_records(&mut app, records)?;
    let viewport = app.log_viewport_height();
    let line_rows = app
        .state
        .current_service()
        .map(|service| service.log.line_index.total_lines())
        .unwrap_or_default()
        - 38;
    assert!(
        line_rows > usize::from(viewport),
        "the line must wrap onto more rows than the pane shows"
    );

    search(&mut app, "needle");
    let frame = draw(&mut app);
    assert!(screen(&frame).contains("/needle 1/1"));
    assert!(is_current(&frame, "needle here")?);
    Ok(())
}

/// On a busy service the view stays on the current match while new lines arrive and the oldest
/// are dropped, wrapped or not.
#[tokio::test]
async fn the_view_stays_on_the_match_while_old_lines_are_dropped() -> eyre::Result<()> {
    let mut app = app(SERVICES)?;
    set_records(&mut app, log_range(0..200, &[150]))?;
    search(&mut app, "needle");
    let row = find(&draw(&mut app), "needle at 150")?.y;

    // Each round twenty lines arrive and as many of the oldest are dropped.
    let mut first = 0;
    for _ in 0..5 {
        first += 20;
        set_records(&mut app, log_range(first..first + 200, &[150]))?;
        let frame = draw(&mut app);
        assert_eq!(find(&frame, "needle at 150")?.y, row);
        assert!(is_current(&frame, "needle at 150")?);
    }

    // Wrapping keeps the top line in place, and so does dropping lines while wrapped.
    press(&mut app, KeyCode::Char('w'));
    let row = find(&draw(&mut app), "needle at 150")?.y;
    for _ in 0..3 {
        first += 10;
        set_records(&mut app, log_range(first..first + 200, &[150]))?;
        assert_eq!(find(&draw(&mut app), "needle at 150")?.y, row);
    }
    Ok(())
}
