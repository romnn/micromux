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
}

/// Terminal width every scenario captures at: the narrowest that keeps the footer's key hints on
/// one row, so the text renders as large as it can in the README.
const COLS: NonZeroU16 = NonZeroU16::new(136).unwrap();
/// Terminal height every scenario captures at.
const ROWS: NonZeroU16 = NonZeroU16::new(22).unwrap();

/// Shrinks the services sidebar from its initial 40 columns to the 28 its rows need, which leaves
/// the rest to the logs.
const NARROW_SIDEBAR: &[u8] = b"------";

pub(crate) const SCENARIOS: &[Scenario] = &[
    // The dashboard: the default frame the config settles into, `api` selected with its logs.
    Scenario {
        name: "overview",
        cols: COLS,
        rows: ROWS,
        keys: &[NARROW_SIDEBAR],
    },
    // Healthcheck diagnostics: navigate down to the unhealthy `payments` service (6 rows below the
    // initial `api` selection), open the healthcheck pane with `H`, then enable wrap with `w` so the
    // long probe command line is readable instead of truncated.
    Scenario {
        name: "healthcheck",
        cols: COLS,
        rows: ROWS,
        keys: &[NARROW_SIDEBAR, b"jjjjjj", b"H", b"w"],
    },
    // Service management: select `web` (one row below `api`) and disable it with `d`. Its sidebar
    // row turns gray (DISABLED) and the process is stopped, while its captured logs remain.
    Scenario {
        name: "disable",
        cols: COLS,
        rows: ROWS,
        keys: &[NARROW_SIDEBAR, b"j", b"d"],
    },
    // Structured logs: select `worker` (two rows below `api`), whose JSON records lead with their
    // timestamps while the fields its config hides stay out of the pane.
    Scenario {
        name: "structured-logs",
        cols: COLS,
        rows: ROWS,
        keys: &[NARROW_SIDEBAR, b"jj"],
    },
    // Mouse selection: a drag over four of `api`'s request lines, sent as SGR mouse reports with
    // one-based columns and rows, highlights them, and the footer offers `y` to copy them.
    // The logs' text starts in column 30, and the 12:00:02 POST line is drawn on row 12.
    Scenario {
        name: "mouse-selection",
        cols: COLS,
        rows: ROWS,
        keys: &[
            NARROW_SIDEBAR,
            b"\x1b[<0;30;12M",
            b"\x1b[<32;69;15M",
            b"\x1b[<0;69;15m",
        ],
    },
    // Service details: `i` opens the popup that describes the selected `api` service in full.
    Scenario {
        name: "service-details",
        cols: COLS,
        rows: ROWS,
        keys: &[NARROW_SIDEBAR, b"i"],
    },
];
