//! The screenshots to capture and the keys that set each one up.

use std::num::NonZeroU16;

/// A single screenshot to capture from the demo config.
pub(crate) struct Scenario {
    /// Output basename (writes `docs/<name>.png`).
    pub(crate) name: &'static str,
    /// Terminal width in columns.
    pub(crate) cols: NonZeroU16,
    /// Terminal height in rows.
    pub(crate) rows: NonZeroU16,
    /// Keystrokes to send (each chunk is settled before the next), e.g. `b"H"` to open the
    /// healthcheck pane. Empty captures the default frame the config settles into.
    pub(crate) keys: &'static [&'static [u8]],
    /// Text the screen must show before the keys are sent, for keys that act on a state the
    /// session reaches only after its first frame.
    pub(crate) ready: Option<&'static str>,
    /// Text the screen must show once the keys were sent, for a frame that waits on something
    /// the keys do not draw at once, such as a reservation's first read.
    pub(crate) shows: Option<&'static str>,
    /// Whether a neighboring worktree's session holds the demo's shared resource meanwhile.
    pub(crate) neighbor: bool,
}

/// Terminal width every scenario captures at: the narrowest that keeps the footer's key hints on
/// one row, so the text renders as large as it can in the README.
const COLS: NonZeroU16 = NonZeroU16::new(146).unwrap();
/// Terminal height every scenario captures at.
const ROWS: NonZeroU16 = NonZeroU16::new(22).unwrap();

/// Shrinks the services sidebar from its initial 40 columns to the 30 its rows need with a column
/// to spare, which leaves the rest to the logs.
const NARROW_SIDEBAR: &[u8] = b"-----";

pub(crate) const SCENARIOS: &[Scenario] = &[
    // The dashboard: the default frame the config settles into, `api` selected with its logs.
    Scenario {
        name: "overview",
        cols: COLS,
        rows: ROWS,
        keys: &[NARROW_SIDEBAR],
        ready: None,
        shows: None,
        neighbor: false,
    },
    // Healthcheck diagnostics: navigate down to the unhealthy `payments` service (6 rows below the
    // initial `api` selection), open the healthcheck pane with `H`, then enable wrap with `w` so the
    // long probe command line is readable instead of truncated.
    Scenario {
        name: "healthcheck",
        cols: COLS,
        rows: ROWS,
        keys: &[NARROW_SIDEBAR, b"jjjjjj", b"H", b"w"],
        ready: None,
        shows: None,
        neighbor: false,
    },
    // Service management: select `web` (one row below `api`) and disable it with `d`. Its sidebar
    // row turns gray (DISABLED) and the process is stopped, while its captured logs remain.
    Scenario {
        name: "disable",
        cols: COLS,
        rows: ROWS,
        keys: &[NARROW_SIDEBAR, b"j", b"d"],
        ready: None,
        shows: None,
        neighbor: false,
    },
    // Structured logs: select `worker` (two rows below `api`), whose JSON records lead with their
    // timestamps while the fields its config hides stay out of the pane.
    Scenario {
        name: "structured-logs",
        cols: COLS,
        rows: ROWS,
        keys: &[NARROW_SIDEBAR, b"jj"],
        ready: None,
        shows: None,
        neighbor: false,
    },
    // Mouse selection: a drag over four of `api`'s request lines, sent as SGR mouse reports with
    // one-based columns and rows, highlights them, and the footer offers `y` to copy them.
    // The logs' text starts in column 32, and the 12:00:02 POST line is drawn on row 12.
    Scenario {
        name: "mouse-selection",
        cols: COLS,
        rows: ROWS,
        keys: &[
            NARROW_SIDEBAR,
            b"\x1b[<0;32;12M",
            b"\x1b[<32;71;15M",
            b"\x1b[<0;71;15m",
        ],
        ready: None,
        shows: None,
        neighbor: false,
    },
    // Service details: `i` opens the popup that describes the selected `api` service in full.
    Scenario {
        name: "service-details",
        cols: COLS,
        rows: ROWS,
        keys: &[NARROW_SIDEBAR, b"i"],
        ready: None,
        shows: None,
        neighbor: false,
    },
    // A shared service this session holds: `e2e` (nine rows below `api`) runs while the session
    // holds `browser`, which its note under the name and the reservation pane above the logs say.
    // The sidebar keeps its width so the note fits.
    Scenario {
        name: "reservation-held",
        cols: COLS,
        rows: ROWS,
        keys: &[b"jjjjjjjjj"],
        ready: None,
        shows: Some("this session"),
        neighbor: false,
    },
    // A shared service another worktree holds: the neighbor took `browser` first, so `e2e` did
    // not start; enabling it with `d` queues it behind the holder, as its notes and the pane say.
    // `d` waits for the refused start, which it would otherwise disable before it is refused.
    Scenario {
        name: "reservation-queued",
        cols: COLS,
        rows: ROWS,
        keys: &[b"jjjjjjjjj", b"d"],
        ready: Some("held by payments-fix"),
        shows: Some("in queue"),
        neighbor: true,
    },
];
