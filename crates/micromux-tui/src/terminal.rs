//! Taking the terminal over and giving it back as it was, on exit, on suspend and on a panic
//! alike.
//!
//! While the TUI shows, the terminal is in raw mode on its alternate screen with bracketed paste,
//! the kitty keyboard protocol where the terminal has it, and, when enabled, mouse capture.
//! Each mode is marked before it is switched on, so [`give_back`] undoes exactly what is on,
//! however far taking the terminal over got, and only once.

use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Once, OnceLock};

#[cfg(windows)]
use crossterm::event::EnableMouseCapture;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use crossterm::{cursor, execute};

/// Whether raw mode is on, so the terminal's own mode comes back.
static RAW: AtomicBool = AtomicBool::new(false);

/// Whether the alternate screen and bracketed paste are on.
static SCREEN: AtomicBool = AtomicBool::new(false);

/// Whether the terminal reports the mouse.
static MOUSE: AtomicBool = AtomicBool::new(false);

/// Whether the TUI pushed its kitty keyboard flags onto the alternate screen's stack, which it
/// does once at most, so a single pop takes them off.
static KEYBOARD: AtomicBool = AtomicBool::new(false);

/// What the TUI asks of the kitty keyboard protocol: Ctrl+Shift+C reported apart from Ctrl-C and
/// Super+C reported at all, so the terminals' copy shortcuts arrive, Alt+Esc apart from Esc, so
/// it leaves PTY input mode, and the shifted key with Shift, so a service in PTY input mode still
/// gets the legacy bytes for Alt+Shift+1.
const KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        .union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS);

/// Whether the terminal has the kitty keyboard protocol.
///
/// It is asked once, before the input reader starts, which would otherwise take the answer.
fn has_keyboard_protocol() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    // A terminal that does not answer has no protocol to speak.
    *SUPPORTED.get_or_init(|| supports_keyboard_enhancement().unwrap_or(false))
}

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

/// Takes the terminal over: raw mode, the alternate screen, bracketed paste, the kitty keyboard
/// protocol where the terminal has it, and mouse capture when `mouse` is set.
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
    // Asked before anything else, so the reader is not running yet.
    has_keyboard_protocol();
    SCREEN.store(true, Ordering::SeqCst);
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    push_keyboard_flags_on(&mut stdout, has_keyboard_protocol())?;
    if mouse {
        MOUSE.store(true, Ordering::SeqCst);
        execute!(stdout, EnableButtonMouseCapture)?;
    }
    Ok(())
}

/// Pushes the kitty keyboard flags where the terminal has the protocol, once at most however
/// often the terminal is taken again, so the single pop in [`give_back`] takes them off.
fn push_keyboard_flags_on(out: &mut impl Write, supported: bool) -> io::Result<()> {
    // The main and alternate screens keep separate flag stacks, so the flags only go onto the
    // alternate screen's, and come off before leaving it.
    if !supported || !SCREEN.load(Ordering::SeqCst) || KEYBOARD.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    execute!(out, PushKeyboardEnhancementFlags(KEYBOARD_FLAGS))
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
    if KEYBOARD.swap(false, Ordering::SeqCst) {
        step(execute!(out, PopKeyboardEnhancementFlags));
    }
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

/// Gives the terminal back and stops micromux's whole process group, as Ctrl-Z does in a shell.
///
/// Raw mode delivers Ctrl-Z as a key instead of the terminal's stop signal, so the TUI sends the
/// signal itself.
/// It signals the group, not just this process, so a wrapper that started micromux, such as
/// `cargo run` or `sh -c`, stops with it and the shell sees the job stop.
/// Services run in sessions and process groups of their own, so they keep running.
/// The signal is `SIGTSTP` rather than `SIGSTOP` because the system ignores `SIGTSTP` in an
/// orphaned process group, where no shell could ever continue it.
#[cfg(unix)]
pub(crate) fn stop_process_group() -> io::Result<()> {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    give_back()?;
    // Pid 0 addresses the caller's process group.
    kill(Pid::from_raw(0), Signal::SIGTSTP).map_err(io::Error::from)
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
    use super::{
        KEYBOARD, MOUSE, RAW, SCREEN, give_back_on, push_keyboard_flags_on, set_mouse_capture_on,
    };
    use color_eyre::eyre;
    use similar_asserts::assert_eq;
    use std::sync::atomic::Ordering;

    const MOUSE_OFF: &str = "\x1b[?1000l";
    const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
    const LEAVE_SCREEN: &str = "\x1b[?1049l";
    const KEYBOARD_POP: &str = "\x1b[<1u";
    const KEYBOARD_PUSH: &str = "\x1b[>5u";

    fn written(out: &[u8]) -> String {
        String::from_utf8_lossy(out).into_owned()
    }

    /// The tests share the process-wide mode flags, so they run as one sequence.
    #[test]
    fn the_terminal_comes_back_exactly_once_with_whatever_was_on() -> eyre::Result<()> {
        // Giving back a terminal with every mode on pops the keyboard flags while still on the
        // alternate screen, whose stack they are on, releases the mouse, leaves the alternate
        // screen and turns raw mode off.
        RAW.store(true, Ordering::SeqCst);
        SCREEN.store(true, Ordering::SeqCst);
        KEYBOARD.store(true, Ordering::SeqCst);
        MOUSE.store(true, Ordering::SeqCst);
        let mut out = Vec::new();
        let mut raw_disabled = 0;
        give_back_on(&mut out, || {
            raw_disabled += 1;
            Ok(())
        })?;
        let out = written(&out);
        assert!(out.contains(MOUSE_OFF), "{out:?}");
        let pop = out.find(KEYBOARD_POP);
        let leave = out.find(LEAVE_SCREEN);
        assert!(pop.is_some() && leave.is_some() && pop < leave, "{out:?}");
        assert_eq!(out.matches(KEYBOARD_POP).count(), 1, "{out:?}");
        assert_eq!(raw_disabled, 1);

        // A second give-back, as the panic hook may run after a normal exit, does nothing.
        let mut again = Vec::new();
        give_back_on(&mut again, || {
            raw_disabled += 1;
            Ok(())
        })?;
        assert!(again.is_empty(), "{:?}", written(&again));
        assert_eq!(raw_disabled, 1);

        // Without the keyboard protocol nothing is popped, and with capture switched off at
        // runtime, giving back leaves mouse modes alone, so a terminal that never captured sees
        // no mouse sequence at all.
        SCREEN.store(true, Ordering::SeqCst);
        MOUSE.store(true, Ordering::SeqCst);
        let mut toggled = Vec::new();
        set_mouse_capture_on(&mut toggled, false)?;
        assert!(written(&toggled).contains(MOUSE_OFF));
        let mut out = Vec::new();
        give_back_on(&mut out, || Ok(()))?;
        assert!(!written(&out).contains("?1000"), "{:?}", written(&out));
        assert!(!written(&out).contains(KEYBOARD_POP), "{:?}", written(&out));

        // Turning capture on twice sends the sequence once.
        let mut out = Vec::new();
        set_mouse_capture_on(&mut out, true)?;
        set_mouse_capture_on(&mut out, true)?;
        assert_eq!(written(&out), MOUSE_ON);
        MOUSE.store(false, Ordering::SeqCst);

        // The keyboard flags go on once however often the terminal is taken, as when it is taken
        // again without having been given back, so the one pop on the way out leaves the stack
        // as it was.
        SCREEN.store(true, Ordering::SeqCst);
        let mut out = Vec::new();
        push_keyboard_flags_on(&mut out, true)?;
        push_keyboard_flags_on(&mut out, true)?;
        give_back_on(&mut out, || Ok(()))?;
        assert_eq!(
            written(&out),
            format!("{KEYBOARD_PUSH}{KEYBOARD_POP}\x1b[?2004l\x1b[?1049l\x1b[?25h")
        );

        // Without the protocol, or off the alternate screen, nothing goes out.
        let mut out = Vec::new();
        SCREEN.store(true, Ordering::SeqCst);
        push_keyboard_flags_on(&mut out, false)?;
        SCREEN.store(false, Ordering::SeqCst);
        push_keyboard_flags_on(&mut out, true)?;
        assert!(out.is_empty(), "{:?}", written(&out));
        assert!(!KEYBOARD.load(Ordering::SeqCst));
        Ok(())
    }
}
