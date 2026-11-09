use color_eyre::eyre;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use micromux::{EVENT_HISTORY, ServiceEvent, ServiceEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::text::Line;
use ratatui::widgets::Widget as _;
use similar_asserts::assert_eq;

use super::{Timeline, clock_time, event_line};
use crate::{App, Focus, RemoteSource, SessionSource, SidePane, ViewOptions};

fn event(seq: u64, kind: ServiceEventKind, detail: &str) -> ServiceEvent {
    ServiceEvent {
        seq,
        at_unix_ms: 1_700_000_000_000 + seq * 1_000,
        run_generation: 1,
        kind,
        detail: detail.to_string(),
        exit_code: None,
        pid: None,
        delay_ms: None,
        blocked_on: None,
    }
}

fn spawned(seq: u64) -> ServiceEvent {
    event(seq, ServiceEventKind::Spawned, "spawned service process")
}

fn seqs(timeline: &Timeline) -> Vec<u64> {
    timeline.since(None).iter().map(|event| event.seq).collect()
}

fn plain(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// The span after the time and its gap, which names the kind.
fn kind_color(line: &Line<'_>) -> Option<Color> {
    line.spans.get(2).and_then(|span| span.style.fg)
}

#[test]
fn reads_forward_from_the_newest_event_held() {
    let mut timeline = Timeline::default();

    // An empty copy reads the whole retained history.
    assert_eq!(timeline.read_after(), None);
    assert!(timeline.extend(vec![spawned(1), spawned(2)]));
    assert_eq!(seqs(&timeline), vec![1, 2]);

    // The next read starts at the newest event held, which the page repeats before what is new.
    assert_eq!(timeline.read_after(), Some(1));
    assert!(timeline.extend(vec![spawned(2), spawned(3)]));
    assert_eq!(seqs(&timeline), vec![1, 2, 3]);

    // A page with only the newest event held means nothing happened since.
    assert!(timeline.extend(vec![spawned(3)]));
    assert_eq!(seqs(&timeline), vec![1, 2, 3]);
    assert_eq!(timeline.since(Some(2)), vec![spawned(3)]);
}

#[test]
fn a_page_that_skips_the_newest_event_held_does_not_continue_the_copy() {
    let mut timeline = Timeline::default();
    timeline.replace(vec![spawned(4), spawned(5)]);

    // Retention dropped event 5 and what followed it before this read.
    assert!(!timeline.extend(vec![spawned(9), spawned(10)]));
    // The service was recreated under its id and numbers its events from one again.
    assert!(!timeline.extend(vec![]));
    assert!(!timeline.extend(vec![event(
        5,
        ServiceEventKind::Created,
        "configured service revived by config reconciliation",
    )]));

    // The copy stays as it was for the caller to replace.
    assert_eq!(seqs(&timeline), vec![4, 5]);
    timeline.replace(vec![spawned(1)]);
    assert_eq!(seqs(&timeline), vec![1]);
}

#[test]
fn holds_no_more_events_than_the_session_retains() {
    let mut timeline = Timeline::default();
    let history = u64::try_from(EVENT_HISTORY).unwrap_or(u64::MAX);
    timeline.replace((1..=history).map(spawned).collect());

    assert!(timeline.extend(vec![spawned(history), spawned(history + 1)]));

    // The oldest event made room for the newest.
    assert_eq!(seqs(&timeline).len(), EVENT_HISTORY);
    assert_eq!(seqs(&timeline).first(), Some(&2));
    assert_eq!(seqs(&timeline).last(), Some(&(history + 1)));
}

#[test]
fn each_event_shows_its_time_kind_and_detail() {
    let mut failed_exit = event(
        3,
        ServiceEventKind::Exited,
        "service process exited with code 1",
    );
    failed_exit.exit_code = Some(1);
    let mut clean_exit = event(
        4,
        ServiceEventKind::Exited,
        "service process exited with code 0",
    );
    clean_exit.exit_code = Some(0);
    let cases = [
        (
            event(1, ServiceEventKind::DependencyBlocked, "waiting for db"),
            "blocked",
            Color::Yellow,
        ),
        (
            event(
                2,
                ServiceEventKind::Spawned,
                "spawned service process with pid 7",
            ),
            "spawned",
            Color::Green,
        ),
        (failed_exit, "exited", Color::Red),
        (clean_exit, "exited", Color::Green),
        (
            event(
                5,
                ServiceEventKind::BackoffScheduled,
                "automatic restart scheduled after 500 ms",
            ),
            "backoff",
            Color::Yellow,
        ),
        (
            event(6, ServiceEventKind::Unhealthy, "service became unhealthy"),
            "unhealthy",
            Color::Red,
        ),
        (
            event(7, ServiceEventKind::CleanupFailed, "cleanup timed out"),
            "cleanup err",
            Color::Red,
        ),
        (
            event(
                8,
                ServiceEventKind::Replaced,
                "dynamic service replaced with revision 2",
            ),
            "replaced",
            Color::Blue,
        ),
        (
            event(
                9,
                ServiceEventKind::RestartRequested,
                "service restart requested",
            ),
            "restart",
            Color::Blue,
        ),
        (
            event(10, ServiceEventKind::Unknown, "from a newer session"),
            "unknown",
            Color::DarkGray,
        ),
    ];

    for (event, label, color) in cases {
        let line = event_line(&event);
        // The kind pads to the widest label, so the details line up in one column.
        assert_eq!(
            plain(&line),
            format!(
                "{} {label:<11} {}",
                clock_time(event.at_unix_ms),
                event.detail
            )
        );
        assert_eq!(kind_color(&line), Some(color), "{label}");
    }
}

#[test]
fn the_text_lists_events_oldest_first_or_says_there_are_none() {
    let mut timeline = Timeline::default();
    assert_eq!(
        timeline.text().lines.iter().map(plain).collect::<Vec<_>>(),
        vec!["no lifecycle events yet".to_string()]
    );

    timeline.replace(vec![
        event(1, ServiceEventKind::RestartRequested, "first"),
        event(2, ServiceEventKind::Spawned, "second"),
    ]);
    let details = timeline
        .text()
        .lines
        .iter()
        .map(|line| {
            plain(line)
                .rsplit(' ')
                .next()
                .unwrap_or_default()
                .to_string()
        })
        .collect::<Vec<_>>();
    assert_eq!(details, vec!["first", "second"]);
}

fn snapshot(id: &str) -> micromux::ServiceSnapshot {
    micromux::ServiceSnapshot::initial(
        id.to_string(),
        id.to_string(),
        Vec::new(),
        None,
        micromux::RestartPolicy::Never,
        vec!["true".to_string()],
        None,
    )
}

/// An attached TUI over an offline mirror, wide enough for whole event lines in the side pane.
fn attached_app(source: RemoteSource) -> App {
    let terminal = source.terminal();
    let mut app = App::new(
        SessionSource::Remote(source),
        terminal,
        micromux::CancellationToken::new(),
        ViewOptions::default(),
    );
    app.handle_crossterm_event(&crossterm::event::Event::Resize(200, 30));
    app
}

/// Mirrors `id` with `events`, then applies the changes that announce it, as the event loop does.
fn mirror(app: &mut App, id: &str, events: Vec<ServiceEvent>) -> eyre::Result<()> {
    let SessionSource::Remote(source) = &app.source else {
        eyre::bail!("the app is not attached");
    };
    source.mirror_service(snapshot(id), events);
    let mut resync = false;
    while let Ok(change) = app.changes.try_recv() {
        resync |= app.apply_change(&change);
    }
    if resync {
        app.resync();
    }
    Ok(())
}

fn press(app: &mut App, ch: char) {
    app.handle_key_press(KeyEvent {
        code: KeyCode::Char(ch),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    });
}

/// The rows of a frame drawn at the app's size, as text.
fn frame(app: &mut App) -> Vec<String> {
    let area = Rect::new(0, 0, app.terminal_cols, app.terminal_rows);
    let mut buffer = Buffer::empty(area);
    app.render(area, &mut buffer);
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .filter_map(|x| buffer.cell((x, y)).map(ratatui::buffer::Cell::symbol))
                .collect()
        })
        .collect()
}

fn shows(rows: &[String], text: &str) -> bool {
    rows.iter().any(|row| row.contains(text))
}

/// `E` shows the events pane in the place of the healthcheck pane, which `H` takes back, and the
/// pane's own key hides it; the focus leaves a pane that hides.
#[tokio::test]
async fn e_toggles_the_events_pane_in_place_of_the_healthcheck_pane() -> eyre::Result<()> {
    let (source, _forwarding) = RemoteSource::offline();
    let mut app = attached_app(source);
    mirror(&mut app, "svc", vec![spawned(1)])?;
    let logs_alone = app.desired_pty_size();

    press(&mut app, 'E');
    assert_eq!(app.side_pane, Some(SidePane::Events));
    let rows = frame(&mut app);
    assert!(shows(&rows, "Events"));
    assert!(shows(&rows, "E Events:ON"));
    assert!(shows(&rows, "H Health:OFF"));

    // Both side panes leave the logs the same width, so the service PTYs fit either.
    let beside_events = app.desired_pty_size();
    assert!(beside_events.0 < logs_alone.0);
    press(&mut app, 'H');
    assert_eq!(app.side_pane, Some(SidePane::Health));
    assert_eq!(app.desired_pty_size(), beside_events);
    assert!(shows(&frame(&mut app), "Healthcheck"));

    // Tab reaches the side pane, whose kind the footer names.
    press(&mut app, 'E');
    app.focus = Focus::Side;
    assert!(shows(&frame(&mut app), "Tab Focus:EVENTS"));
    press(&mut app, 'E');
    assert_eq!(app.side_pane, None);
    assert_eq!(app.focus, Focus::Logs);
    assert_eq!(app.desired_pty_size(), logs_alone);
    Ok(())
}

/// Opening the events pane in an attached TUI only watches, so it leaves the session's PTYs
/// alone, as the healthcheck pane does.
#[tokio::test]
async fn the_events_pane_never_resizes_an_attached_sessions_ptys() -> eyre::Result<()> {
    let (source, forwarding) = RemoteSource::offline();
    let mut app = attached_app(source);

    press(&mut app, 'E');
    frame(&mut app);
    press(&mut app, 'E');

    assert!(!forwarding.size.has_changed()?);
    Ok(())
}

/// The pane shows the selected service's events as they arrive, reading only the new ones, and
/// starts over when the service's history no longer continues what it read.
#[tokio::test]
async fn the_events_pane_follows_the_selected_services_timeline() -> eyre::Result<()> {
    let (source, _forwarding) = RemoteSource::offline();
    let mut app = attached_app(source);
    mirror(
        &mut app,
        "svc",
        vec![
            event(1, ServiceEventKind::DependencyBlocked, "waiting for db"),
            event(
                2,
                ServiceEventKind::Spawned,
                "spawned service process with pid 7",
            ),
        ],
    )?;
    press(&mut app, 'E');
    let rows = frame(&mut app);
    assert!(shows(&rows, "blocked     waiting for db"));
    assert!(shows(
        &rows,
        "spawned     spawned service process with pid 7"
    ));

    // A new event extends the copy the pane read; it does not re-read what it holds.
    let mut exited = event(
        3,
        ServiceEventKind::Exited,
        "service process exited with code 1",
    );
    exited.exit_code = Some(1);
    mirror(
        &mut app,
        "svc",
        vec![
            event(1, ServiceEventKind::DependencyBlocked, "waiting for db"),
            event(
                2,
                ServiceEventKind::Spawned,
                "spawned service process with pid 7",
            ),
            exited,
        ],
    )?;
    let rows = frame(&mut app);
    assert!(shows(
        &rows,
        "exited      service process exited with code 1"
    ));
    let service = app
        .state
        .current_service()
        .ok_or_else(|| eyre::eyre!("no service"))?;
    assert_eq!(service.timeline.read_after(), Some(2));
    assert!(!service.events_dirty);

    // The service was recreated under its id, so its events count from one again.
    mirror(
        &mut app,
        "svc",
        vec![event(
            1,
            ServiceEventKind::Created,
            "configured service revived",
        )],
    )?;
    let rows = frame(&mut app);
    assert!(shows(&rows, "configured service revived"));
    assert!(!shows(&rows, "waiting for db"));
    Ok(())
}

/// Selecting another service shows that service's events, and selecting the first again shows
/// its events once more.
#[tokio::test]
async fn the_events_pane_switches_with_the_selected_service() -> eyre::Result<()> {
    let (source, _forwarding) = RemoteSource::offline();
    let mut app = attached_app(source);
    mirror(
        &mut app,
        "api",
        vec![event(1, ServiceEventKind::Healthy, "api became healthy")],
    )?;
    mirror(
        &mut app,
        "worker",
        vec![event(
            1,
            ServiceEventKind::BackoffScheduled,
            "worker backing off",
        )],
    )?;
    press(&mut app, 'E');

    let rows = frame(&mut app);
    assert!(shows(&rows, "api became healthy"));
    assert!(!shows(&rows, "worker backing off"));

    press(&mut app, 'j');
    let rows = frame(&mut app);
    assert!(shows(&rows, "worker backing off"));
    assert!(!shows(&rows, "api became healthy"));

    press(&mut app, 'k');
    assert!(shows(&frame(&mut app), "api became healthy"));
    Ok(())
}

/// An attached session too old to report events says so instead of claiming there are none.
#[tokio::test]
async fn the_events_pane_explains_an_older_session() -> eyre::Result<()> {
    let (source, _forwarding) =
        RemoteSource::offline_at(micromux_control::ProtocolVersion::new(3, 2));
    let mut app = attached_app(source);
    mirror(&mut app, "svc", Vec::new())?;
    press(&mut app, 'E');

    let rows = frame(&mut app);
    assert!(shows(&rows, "this session predates lifecycle events"));
    assert!(!shows(&rows, "no lifecycle events yet"));
    Ok(())
}
