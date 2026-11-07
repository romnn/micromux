//! Taking the terminal over and giving it back as it was, on exit, on suspend and on a panic
//! alike.
//!
//! While the TUI shows, the terminal is in raw mode on its alternate screen with bracketed paste
//! and, when enabled, mouse capture.
//! Each mode is marked before it is switched on, so [`give_back`] undoes exactly what is on,
//! however far taking the terminal over got, and only once.

use std::fmt;
use std::io::{self, Write};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(windows)]
use crossterm::event::EnableMouseCapture;
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor, execute};

/// Whether raw mode is on, so the terminal's own mode comes back.
static RAW: AtomicBool = AtomicBool::new(false);

/// Whether the alternate screen and bracketed paste are on.
static SCREEN: AtomicBool = AtomicBool::new(false);

/// Whether the terminal reports the mouse.
static MOUSE: AtomicBool = AtomicBool::new(false);

/// Mouse reporting for presses, releases, drags and the wheel, in SGR encoding.
///
/// Unlike crossterm's [`EnableMouseCapture`](crossterm::event::EnableMouseCapture), it leaves out any-event tracking, which reports
/// every move of the pointer and would wake the TUI for a frame each time.
struct EnableButtonMouseCapture;

impl crossterm::Command for EnableButtonMouseCapture {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        // Normal tracking, button-event tracking for drags, and SGR coordinates.
        f.write_str("\x1b[?1000h\x1b[?1002h\x1b[?1006h")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        EnableMouseCapture.execute_winapi()
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        false
    }
}

/// Takes the terminal over: raw mode, the alternate screen, bracketed paste, and mouse capture
/// when `mouse` is set.
///
/// It also takes the terminal again after a suspend, setting every mode anew, since the shell
/// may have put its own modes back meanwhile.
pub(crate) fn take_over(mouse: bool) -> io::Result<()> {
    let mut stdout = io::stdout();
    // crossterm counts raw mode as on even when a shell turned it off during a suspend.
    if RAW.swap(true, Ordering::SeqCst) {
        disable_raw_mode()?;
    }
    enable_raw_mode()?;
    SCREEN.store(true, Ordering::SeqCst);
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    if mouse {
        MOUSE.store(true, Ordering::SeqCst);
        execute!(stdout, EnableButtonMouseCapture)?;
    }
    Ok(())
}

/// Turns mouse capture on or off while the TUI shows.
pub(crate) fn set_mouse_capture(enabled: bool) -> io::Result<()> {
    set_mouse_capture_on(&mut io::stdout(), enabled)
}

fn set_mouse_capture_on(out: &mut impl Write, enabled: bool) -> io::Result<()> {
    if MOUSE.swap(enabled, Ordering::SeqCst) == enabled {
        return Ok(());
    }
    if enabled {
        execute!(out, EnableButtonMouseCapture)
    } else {
        execute!(out, DisableMouseCapture)
    }
}

/// Gives the terminal back as it was, once, however far taking it over got.
///
/// Every step runs even when an earlier one fails, and the first failure is returned.
pub(crate) fn give_back() -> io::Result<()> {
    give_back_on(&mut io::stdout(), disable_raw_mode)
}

fn give_back_on(
    out: &mut impl Write,
    disable_raw: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let mut first: io::Result<()> = Ok(());
    let mut step = |result: io::Result<()>| {
        if first.is_ok() {
            first = result;
        }
    };
    if MOUSE.swap(false, Ordering::SeqCst) {
        step(execute!(out, DisableMouseCapture));
    }
    if SCREEN.swap(false, Ordering::SeqCst) {
        step(execute!(
            out,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            cursor::Show
        ));
    }
    if RAW.swap(false, Ordering::SeqCst) {
        step(disable_raw());
    }
    first
}

/// Installs, once per process, a panic hook that gives the terminal back before the panic's
/// report, so the report is readable and the shell is not left capturing the mouse.
pub(crate) fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let report = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // A failure here cannot be reported anywhere better than the panic's own report.
            give_back().ok();
            report(info);
        }));
    });
}

/// Suspends the process as Ctrl-Z does in a shell: gives the terminal back, stops until the
/// shell continues it, then takes the terminal over again with mouse capture as `mouse` says.
///
/// Raw mode delivers Ctrl-Z as a key instead of the terminal's stop signal, so the TUI raises
/// the signal itself.
/// It raises `SIGTSTP` rather than `SIGSTOP` because the system ignores `SIGTSTP` in an orphaned
/// process group, where no shell could ever continue the process; the call then returns at once
/// and the TUI keeps running.
#[cfg(unix)]
pub(crate) fn suspend(mouse: bool) -> io::Result<()> {
    give_back()?;
    // `raise` signals this thread, so the stop takes effect before it returns.
    nix::sys::signal::raise(nix::sys::signal::Signal::SIGTSTP).map_err(io::Error::from)?;
    take_over(mouse)
}

/// Notices that the process continued after a stop, such as one sent from outside, which leaves
/// the terminal as the shell left it.
pub(crate) struct Continued {
    #[cfg(unix)]
    signal: Option<tokio::signal::unix::Signal>,
}

impl Continued {
    /// Starts watching; without a way to watch, [`Self::recv`] never returns.
    pub(crate) fn watch() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let kind = SignalKind::from_raw(nix::sys::signal::Signal::SIGCONT as i32);
            Self {
                signal: signal(kind).ok(),
            }
        }
        #[cfg(not(unix))]
        Self {}
    }

    /// Waits until the process continues after a stop.
    pub(crate) async fn recv(&mut self) {
        #[cfg(unix)]
        if let Some(signal) = self.signal.as_mut()
            && signal.recv().await.is_some()
        {
            return;
        }
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::{MOUSE, RAW, SCREEN, give_back_on, set_mouse_capture_on};
    use color_eyre::eyre;
    use similar_asserts::assert_eq;
    use std::sync::atomic::Ordering;

    const MOUSE_OFF: &str = "\x1b[?1000l";
    const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
    const LEAVE_SCREEN: &str = "\x1b[?1049l";

    fn written(out: &[u8]) -> String {
        String::from_utf8_lossy(out).into_owned()
    }

    /// The tests share the process-wide mode flags, so they run as one sequence.
    #[test]
    fn the_terminal_comes_back_exactly_once_with_whatever_was_on() -> eyre::Result<()> {
        // Giving back a terminal with every mode on releases the mouse, leaves the alternate
        // screen and turns raw mode off.
        RAW.store(true, Ordering::SeqCst);
        SCREEN.store(true, Ordering::SeqCst);
        MOUSE.store(true, Ordering::SeqCst);
        let mut out = Vec::new();
        let mut raw_disabled = 0;
        give_back_on(&mut out, || {
            raw_disabled += 1;
            Ok(())
        })?;
        let out = written(&out);
        assert!(out.contains(MOUSE_OFF), "{out:?}");
        assert!(out.contains(LEAVE_SCREEN), "{out:?}");
        assert_eq!(raw_disabled, 1);

        // A second give-back, as the panic hook may run after a normal exit, does nothing.
        let mut again = Vec::new();
        give_back_on(&mut again, || {
            raw_disabled += 1;
            Ok(())
        })?;
        assert!(again.is_empty(), "{:?}", written(&again));
        assert_eq!(raw_disabled, 1);

        // With capture switched off at runtime, giving back leaves mouse modes alone, so a
        // terminal that never captured sees no mouse sequence at all.
        SCREEN.store(true, Ordering::SeqCst);
        MOUSE.store(true, Ordering::SeqCst);
        let mut toggled = Vec::new();
        set_mouse_capture_on(&mut toggled, false)?;
        assert!(written(&toggled).contains(MOUSE_OFF));
        let mut out = Vec::new();
        give_back_on(&mut out, || Ok(()))?;
        assert!(!written(&out).contains("?1000"), "{:?}", written(&out));

        // Turning capture on twice sends the sequence once.
        let mut out = Vec::new();
        set_mouse_capture_on(&mut out, true)?;
        set_mouse_capture_on(&mut out, true)?;
        assert_eq!(written(&out), MOUSE_ON);
        MOUSE.store(false, Ordering::SeqCst);
        Ok(())
    }
}
