//! Where this session stands with a shared resource, as the services list, the reservation pane
//! and the info popup show it.
//!
//! Every view draws from the session's own reply to a status read.
//! Only that reply knows which claim is the session's and whether its scheduler hands the
//! resource over; the ledger file alone knows neither.

use micromux::{Claim, ReservationReply, ReservationState};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// The latest status read of a resource: the session's reply, or why it could not be read.
pub(crate) type ReservationRead = Result<ReservationReply, String>;

/// How often the TUI reads where the session stands with each resource its services name.
pub(crate) const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// The distinct resources the services name, in roster order.
pub(crate) fn named_resources<'a>(
    snapshots: impl IntoIterator<Item = &'a micromux::ServiceSnapshot>,
) -> Vec<String> {
    let mut resources: Vec<String> = Vec::new();
    for snapshot in snapshots {
        if let Some(resource) = &snapshot.resource
            && !resources.contains(resource)
        {
            resources.push(resource.clone());
        }
    }
    resources
}

/// Where this session stands with a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Standing<'a> {
    /// The session holds the resource, its turn `protected` until its deadline and over after
    /// it, when it hands over as soon as another session queues.
    Ours { protected: bool },
    /// The session stops its services that need the resource, then passes it on.
    HandingOver,
    /// The session waits at one-based `position` of the `waiting` sessions.
    Queued { position: usize, waiting: usize },
    /// Another session holds the resource.
    HeldElsewhere { holder: &'a Claim },
    /// Nobody holds the resource, but sessions wait for it and go first.
    Pending { next: &'a Claim },
    /// Nobody holds or waits for the resource.
    Free,
    /// The session's reply has not arrived or could not be read.
    Unknown,
}

impl<'a> Standing<'a> {
    fn of(read: Option<&'a ReservationRead>, now_ms: u64) -> Self {
        let Some(Ok(reply)) = read else {
            return Self::Unknown;
        };
        match reply.state {
            ReservationState::Granted => Self::Ours {
                protected: reply
                    .view
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner.claim.renew_by_unix_ms > now_ms),
            },
            ReservationState::Releasing => Self::HandingOver,
            ReservationState::Waiting { position } => Self::Queued {
                position,
                waiting: reply.view.waiters.len(),
            },
            ReservationState::Released => match (&reply.view.owner, reply.view.waiters.first()) {
                (Some(owner), _) => Self::HeldElsewhere {
                    holder: &owner.claim,
                },
                (None, Some(next)) => Self::Pending { next },
                (None, None) => Self::Free,
            },
        }
    }
}

/// The services list's rows under a service that needs a resource, each at most `width` cells:
/// until when this session's turn is protected, or who else holds or waits for the resource,
/// then the rest, cut from the end to fit; a queued start's place takes a row of its own.
///
/// The process state above already says whether the service runs or waits, so the rows say
/// only what the state cannot, in gray, so they do not compete with the state.
pub(crate) fn list_notes(
    read: Option<&ReservationRead>,
    now_ms: u64,
    width: usize,
) -> Vec<Line<'static>> {
    let standing = Standing::of(read, now_ms);
    let view = match read {
        Some(Ok(reply)) => Some(&reply.view),
        Some(Err(_)) | None => None,
    };
    let owner = view.and_then(|view| view.owner.as_ref());
    let dim = Style::new().fg(Color::DarkGray);
    let note = |text: String| Span::styled(text, dim);
    // The protected turn's end stands out in italics wherever it shows.
    let until = owner
        .map(|owner| owner.claim.renew_by_unix_ms)
        .filter(|renew_by| *renew_by > now_ms)
        .map(|renew_by| {
            Span::styled(
                format!("until {}", clock(renew_by)),
                dim.add_modifier(Modifier::ITALIC),
            )
        });
    let waiting = view
        .map(|view| view.waiters.len())
        .filter(|waiting| *waiting > 0)
        .map(|waiting| note(format!("{waiting} waiting")));
    let held_by = owner.map(|owner| note(format!("held by {}", owner.claim.session_name)));
    // This session's own turn leads with its end, another session's with who holds it.
    let (lead, rest) = match standing {
        Standing::Ours { protected: true } => (until, vec![waiting]),
        Standing::Ours { protected: false } => (Some(note("turn over".to_string())), vec![waiting]),
        Standing::HandingOver => (Some(note("handing over".to_string())), vec![waiting]),
        Standing::Queued { position, waiting } => {
            // The holder and the place each take a row, so neither is cut.
            let place = [
                format!("{position}/{waiting} in queue"),
                format!("{position}/{waiting}"),
                format!("#{position}"),
            ]
            .into_iter()
            // A rank cut short would read as another, so none shows where none fits.
            .find(|place| Line::raw(place.as_str()).width() <= width)
            .unwrap_or_default();
            return held_by
                .into_iter()
                .chain([note(place)])
                .map(Line::from)
                .collect();
        }
        Standing::HeldElsewhere { .. } => (held_by, vec![until, waiting]),
        Standing::Pending { next } => {
            (Some(note(format!("{} is next", next.session_name))), vec![])
        }
        Standing::Free => (Some(note("free".to_string())), vec![]),
        Standing::Unknown => (Some(note("…".to_string())), vec![]),
    };
    let mut row = Line::from(lead.unwrap_or_default());
    // The parts after the lead show while they fit, each after a separator.
    for part in rest.into_iter().flatten() {
        if row.width() + 3 + part.width() > width {
            break;
        }
        row.push_span(Span::styled(" · ", dim));
        row.push_span(part);
    }
    vec![row]
}

/// The reservation's facts as labeled cells, for the pane's table and the info popup: who holds
/// the resource, since when and until when it is protected, then this session's place, the
/// queue and the holder's reason.
///
/// The pane shows each cell's first row; with `full`, as in the info popup, the holder names its
/// worktree and the queue lists every waiter on a row of its own with its reason.
pub(crate) fn detail_cells(
    read: Option<&ReservationRead>,
    now_ms: u64,
    home: Option<&std::ffi::OsStr>,
    full: bool,
) -> Vec<(&'static str, Vec<Line<'static>>)> {
    let dim = Style::new().fg(Color::DarkGray);
    let none = || vec![Line::styled("—", dim)];
    let standing = Standing::of(read, now_ms);
    let reply = match read {
        Some(Ok(reply)) => reply,
        Some(Err(err)) => {
            return vec![(
                "Status",
                vec![Line::styled(format!("unreadable: {err}"), dim)],
            )];
        }
        None => return vec![("Status", vec![Line::styled("reading…", dim)])],
    };
    let view = &reply.view;
    let ours = matches!(standing, Standing::Ours { .. } | Standing::HandingOver);
    let owner = view.owner.as_ref();

    let holder = holder_cell(owner, ours, home, full);
    let since = owner.map_or_else(none, |owner| {
        vec![Line::from(vec![
            Span::raw(clock(owner.granted_at_unix_ms)),
            Span::styled(
                format!(
                    " ({} ago)",
                    time_left(now_ms.saturating_sub(owner.granted_at_unix_ms))
                ),
                dim,
            ),
        ])]
    });
    let until = owner.map_or_else(none, |owner| {
        let renew_by = owner.claim.renew_by_unix_ms;
        vec![match (
            standing,
            renew_by.checked_sub(now_ms).filter(|left| *left > 0),
        ) {
            (Standing::HandingOver, _) => {
                Line::styled("handing over", Style::new().fg(Color::Magenta))
            }
            (_, Some(left_ms)) => Line::from(vec![
                Span::raw(clock(renew_by)),
                Span::styled(format!(" ({} left)", time_left(left_ms)), dim),
            ]),
            (_, None) => Line::styled("turn over", Style::new().fg(Color::Yellow)),
        }]
    });
    // A place in the queue, which the holder and a session that does not queue have none of.
    let place = match standing {
        Standing::Queued { position, waiting } => Line::styled(
            format!("{position}/{waiting}"),
            Style::new().fg(Color::Yellow),
        ),
        _ => Line::styled("—", dim),
    };

    let queue = queue_cell(reply, home, full);
    let reason = owner
        .and_then(|owner| request_terms(&owner.claim))
        .map_or_else(none, |terms| vec![Line::raw(terms)]);

    vec![
        ("Holder", vec![holder]),
        ("Since", since),
        ("Until", until),
        ("Place", vec![place]),
        ("Queue", queue),
        ("Reason", reason),
    ]
}

/// Who holds the resource: this session, another one, with its worktree in `full`, or nobody.
fn holder_cell(
    owner: Option<&micromux::Owner>,
    ours: bool,
    home: Option<&std::ffi::OsStr>,
    full: bool,
) -> Line<'static> {
    let dim = Style::new().fg(Color::DarkGray);
    match owner {
        Some(owner) if ours && full => Line::from(vec![
            Span::styled("this session", Style::new().fg(Color::Green)),
            Span::styled(format!(" ({})", worktree(&owner.claim, home)), dim),
        ]),
        Some(_) if ours => Line::styled("this session", Style::new().fg(Color::Green)),
        Some(owner) if full => Line::from(vec![
            Span::styled(
                owner.claim.session_name.clone(),
                Style::new().fg(Color::LightRed),
            ),
            Span::styled(format!(" ({})", worktree(&owner.claim, home)), dim),
        ]),
        Some(owner) => Line::styled(
            owner.claim.session_name.clone(),
            Style::new().fg(Color::LightRed),
        ),
        None => Line::styled("nobody", Style::new().fg(Color::LightBlue)),
    }
}

/// The queue in order, this session marked: one waiter a row with its worktree, since when and
/// why in `full`, else the names alone, first to last.
fn queue_cell(
    reply: &ReservationReply,
    home: Option<&std::ffi::OsStr>,
    full: bool,
) -> Vec<Line<'static>> {
    let dim = Style::new().fg(Color::DarkGray);
    let view = &reply.view;
    let own = reply.own_claim();
    let name = |claim: &Claim| {
        if own.is_some_and(|own| own.id == claim.id) {
            Span::styled("this session", Style::new().fg(Color::Yellow))
        } else {
            Span::raw(claim.session_name.clone())
        }
    };
    if view.waiters.is_empty() {
        vec![Line::styled("empty", dim)]
    } else if full {
        view.waiters
            .iter()
            .enumerate()
            .map(|(index, claim)| {
                let mut line = Line::from(vec![
                    Span::styled(format!("{}. ", index + 1), dim),
                    name(claim),
                    Span::styled(
                        format!(
                            " ({}) since {}",
                            worktree(claim, home),
                            clock(claim.requested_at_unix_ms)
                        ),
                        dim,
                    ),
                ]);
                if let Some(terms) = request_terms(claim) {
                    line.push_span(Span::styled(format!(" · {terms}"), dim));
                }
                line
            })
            .collect()
    } else {
        let mut line = Line::default();
        for (index, claim) in view.waiters.iter().enumerate() {
            if index > 0 {
                line.push_span(Span::styled(" > ", dim));
            }
            line.push_span(name(claim));
        }
        vec![line]
    }
}

/// The worktree a session's config lives in, relative to the home directory.
fn worktree(claim: &Claim, home: Option<&std::ffi::OsStr>) -> String {
    std::path::Path::new(&claim.config_path)
        .parent()
        .map_or_else(String::new, |dir| {
            crate::render::home_relative(&dir.to_string_lossy(), home)
        })
}

/// A request's reason and estimate, when it gave either, such as `e2e run (~45m)`.
fn request_terms(claim: &Claim) -> Option<String> {
    let estimate = claim
        .estimate_secs
        .map(|secs| format!("~{}", micromux::duration_label(secs.saturating_mul(1000))));
    match (&claim.reason, estimate) {
        (Some(reason), Some(estimate)) => Some(format!("{reason} ({estimate})")),
        (Some(reason), None) => Some(reason.clone()),
        (None, estimate) => estimate,
    }
}

/// How long a turn stays protected, in minutes, or `<1m` in its last minute.
fn time_left(left_ms: u64) -> String {
    if left_ms < 60_000 {
        "<1m".to_string()
    } else {
        micromux::duration_label(left_ms)
    }
}

/// A Unix millisecond timestamp as the local clock time, such as `19:37`.
fn clock(unix_ms: u64) -> String {
    i64::try_from(unix_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

/// When a relative time the views show changes next: a holder's turn ending, and otherwise half
/// a minute from now while any holder shows how long it has held its resource.
pub(crate) fn next_change<'a>(
    reads: impl IntoIterator<Item = &'a ReservationRead>,
    now_ms: u64,
) -> Option<std::time::Duration> {
    reads
        .into_iter()
        .filter_map(|read| read.as_ref().ok()?.view.owner.as_ref())
        .map(|owner| {
            let left_ms = owner
                .claim
                .renew_by_unix_ms
                .checked_sub(now_ms)
                .filter(|left| *left > 0)
                .unwrap_or(u64::MAX);
            std::time::Duration::from_millis(left_ms.min(30_000))
        })
        .min()
}

/// A reply about resource `desk` in state `state`, held by `owner` since five minutes before
/// `now_ms` and protected for an hour after it, with `waiters` in order, where `own` names this
/// session's request.
#[cfg(test)]
pub(crate) fn test_reply(
    state: ReservationState,
    owner: Option<&str>,
    waiters: &[&str],
    own: Option<&str>,
    now_ms: u64,
) -> ReservationReply {
    let claim = |name: &str| Claim {
        id: format!("{name}-claim"),
        session_id: format!("{name}-id"),
        instance: format!("{name}-instance"),
        session_name: name.to_string(),
        config_path: format!("/work/{name}/micromux.yaml"),
        estimate_secs: None,
        reason: None,
        requested_at_unix_ms: now_ms - 60_000,
        renew_by_unix_ms: now_ms + 3_600_000,
    };
    ReservationReply {
        view: micromux::ResourceView {
            name: "desk".to_string(),
            owner: owner.map(|name| micromux::Owner {
                claim: claim(name),
                granted_at_unix_ms: now_ms - 300_000,
            }),
            waiters: waiters.iter().map(|name| claim(name)).collect(),
        },
        claim: own.map(|name| format!("{name}-claim")),
        state,
    }
}

#[cfg(test)]
mod tests {
    use micromux::ReservationState::{Granted, Released, Releasing, Waiting};
    use similar_asserts::assert_eq;

    use super::*;

    const NOW: u64 = 1_800_000_000_000;
    const HOUR: u64 = 3_600_000;

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    fn notes(read: &ReservationRead, now_ms: u64, width: usize) -> Vec<String> {
        texts(&list_notes(Some(read), now_ms, width))
    }

    /// The list's notes say whose the resource is and for how long, which the process state
    /// cannot, and drop or shorten what does not fit.
    #[test]
    fn list_notes_say_whose_the_resource_is() {
        let until = format!("until {}", clock(NOW + HOUR));

        // This session's protected turn, then with sessions waiting behind it; a part that does
        // not fit is left out.
        let ours = Ok(test_reply(Granted, Some("own"), &[], Some("own"), NOW));
        assert_eq!(notes(&ours, NOW, 40), [until.as_str()]);
        let contested = Ok(test_reply(
            Granted,
            Some("own"),
            &["a", "b"],
            Some("own"),
            NOW,
        ));
        assert_eq!(notes(&contested, NOW, 40), [format!("{until} · 2 waiting")]);
        assert_eq!(notes(&contested, NOW, 15), [until.as_str()]);

        // The turn's end is italic, the rest plain.
        let lines = list_notes(Some(&contested), NOW, 40);
        let styles = lines
            .first()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.style.add_modifier.contains(Modifier::ITALIC))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert_eq!(styles, [true, false, false]);

        // A turn that is over, and one being handed over.
        assert_eq!(notes(&ours, NOW + 2 * HOUR, 40), ["turn over"]);
        let releasing = Ok(test_reply(Releasing, Some("own"), &["a"], Some("own"), NOW));
        assert_eq!(notes(&releasing, NOW, 40), ["handing over · 1 waiting"]);

        // A queued start: the holder and the place each take a row, the place shortened to fit.
        let queued = Ok(test_reply(
            Waiting { position: 2 },
            Some("x"),
            &["y", "own"],
            Some("own"),
            NOW,
        ));
        assert_eq!(notes(&queued, NOW, 40), ["held by x", "2/2 in queue"]);
        assert_eq!(notes(&queued, NOW, 8), ["held by x", "2/2"]);
        assert_eq!(notes(&queued, NOW, 2), ["held by x", "#2"]);
        assert_eq!(notes(&queued, NOW, 1), ["held by x", ""]);

        // Another session's turn, a queue nobody holds yet, a free resource, and no read.
        let elsewhere = Ok(test_reply(Released, Some("x"), &[], None, NOW));
        assert_eq!(notes(&elsewhere, NOW, 40), [format!("held by x · {until}")]);
        let pending = Ok(test_reply(Released, None, &["y"], None, NOW));
        assert_eq!(notes(&pending, NOW, 40), ["y is next"]);
        let free = Ok(test_reply(Released, None, &[], None, NOW));
        assert_eq!(notes(&free, NOW, 40), ["free"]);
        assert_eq!(texts(&list_notes(None, NOW, 40)), ["…"]);
    }

    fn cells(read: Option<&ReservationRead>, full: bool) -> Vec<(&'static str, Vec<String>)> {
        detail_cells(read, NOW, None, full)
            .into_iter()
            .map(|(label, values)| (label, texts(&values)))
            .collect()
    }

    fn row(label: &'static str, values: &[&str]) -> (&'static str, Vec<String>) {
        (label, values.iter().map(ToString::to_string).collect())
    }

    /// The pane tabulates who holds the resource, when, this session's place and the queue,
    /// and the info popup adds the worktrees and every waiter's row.
    #[test]
    fn detail_cells_tabulate_the_reservation() {
        let since = format!("{} (5m ago)", clock(NOW - 300_000));
        let until = format!("{} (1h left)", clock(NOW + HOUR));

        // This session holds the resource and nobody waits.
        let held = Ok(test_reply(Granted, Some("own"), &[], Some("own"), NOW));
        assert_eq!(
            cells(Some(&held), false),
            [
                row("Holder", &["this session"]),
                row("Since", &[&since]),
                row("Until", &[&until]),
                row("Place", &["—"]),
                row("Queue", &["empty"]),
                row("Reason", &["—"]),
            ]
        );

        // Another session holds it for a reason, and this session waits second.
        let mut queued = Ok(test_reply(
            Waiting { position: 2 },
            Some("x"),
            &["y", "own"],
            Some("own"),
            NOW,
        ));
        if let Ok(reply) = &mut queued
            && let Some(owner) = &mut reply.view.owner
        {
            owner.claim.reason = Some("e2e run".to_string());
            owner.claim.estimate_secs = Some(2700);
        }
        assert_eq!(
            cells(Some(&queued), false),
            [
                row("Holder", &["x"]),
                row("Since", &[&since]),
                row("Until", &[&until]),
                row("Place", &["2/2"]),
                row("Queue", &["y > this session"]),
                row("Reason", &["e2e run (~45m)"]),
            ]
        );
        let requested = clock(NOW - 60_000);
        let full = cells(Some(&queued), true);
        assert_eq!(full.first(), Some(&row("Holder", &["x (/work/x)"])));
        assert_eq!(
            full.get(4),
            Some(&row(
                "Queue",
                &[
                    &format!("1. y (/work/y) since {requested}"),
                    &format!("2. this session (/work/own) since {requested}"),
                ]
            ))
        );

        // Before the first read, and when it failed.
        assert_eq!(cells(None, false), [row("Status", &["reading…"])]);
        let failed: ReservationRead = Err("no session".to_string());
        assert_eq!(
            cells(Some(&failed), false),
            [row("Status", &["unreadable: no session"])]
        );
    }

    /// The views redraw when a protected turn ends, and every half minute while a holder shows
    /// how long it has held its resource, but not while nobody holds one.
    #[test]
    fn redraws_follow_the_turns_that_run_out() {
        let ours = Ok(test_reply(Granted, Some("own"), &[], Some("own"), NOW));
        let ending = Ok(test_reply(
            Granted,
            Some("own"),
            &[],
            Some("own"),
            NOW - HOUR + 10_000,
        ));
        let seconds = |reads: Vec<ReservationRead>, now_ms| {
            next_change(&reads, now_ms).map(|after| after.as_secs())
        };
        assert_eq!(seconds(vec![ours.clone()], NOW), Some(30));
        assert_eq!(seconds(vec![ours.clone(), ending], NOW), Some(10));
        assert_eq!(seconds(vec![ours], NOW + 2 * HOUR), Some(30));
        assert_eq!(
            seconds(vec![Ok(test_reply(Released, None, &[], None, NOW))], NOW),
            None
        );
    }
}
