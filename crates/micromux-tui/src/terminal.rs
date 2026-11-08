//! Taking the terminal over and giving it back as it was, on exit, on suspend and on a panic
//! alike.
//!
//! While the TUI shows, the terminal is in raw mode on its alternate screen with bracketed paste,
//! the kitty keyboard protocol where the terminal has it, and, when enabled, mouse capture.
//! Each mode is marked before it is switched on, so [`give_back`] undoes exactly what is on,
//! however far taking the terminal over got, and only once.

use std::fmt;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Once, OnceLock};

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

/// Which keyboard reporting the TUI asked for, as a [`Keyboard`], which it does once at most, so
/// a single reset takes it back.
static KEYBOARD: AtomicU8 = AtomicU8::new(Keyboard::Legacy as u8);

/// How the terminal reports keys micromux asked it to report apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Keyboard {
    /// As terminals do by default, Ctrl+Shift+C as Ctrl-C.
    Legacy = 0,
    /// The kitty keyboard protocol's flags, pushed onto the alternate screen's stack.
    Kitty = 1,
    /// xterm's modifyOtherKeys at level 2, which tmux turns into CSI u reports.
    ModifyOtherKeys = 2,
}

/// The sequences that ask for modifyOtherKeys at level 2 and reset it.
const MODIFY_OTHER_KEYS_ON: &[u8] = b"\x1b[>4;2m";
const MODIFY_OTHER_KEYS_RESET: &[u8] = b"\x1b[>4m";

/// What the TUI asks of the kitty keyboard protocol: Ctrl+Shift+C reported apart from Ctrl-C and
/// Super+C reported at all, so the terminals' copy shortcuts arrive, and the shifted key with
/// Shift, so keys reported just before PTY input mode turns the protocol off still fold into the
/// legacy bytes for Alt+Shift+1.
const KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        .union(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS);

/// Whether the terminal has the kitty keyboard protocol.
///
/// It is asked once, before the input reader starts, which would otherwise take the answer.
fn has_keyboard_protocol() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let answer = supports_keyboard_enhancement();
        let message = match &answer {
            Ok(true) => "the terminal has the kitty keyboard protocol".to_string(),
            Ok(false) => "the terminal has no kitty keyboard protocol".to_string(),
            Err(err) => format!(
                "the terminal did not say whether it has the kitty keyboard protocol: {err}"
            ),
        };
        log_keyboard(&message);
        // A terminal that does not answer has no protocol to speak.
        answer.unwrap_or(false)
    })
}

/// Whether `MICROMUX_DEBUG_KEYS` asks for every key micromux reads, and what it learned about
/// the keyboard, to go to its log.
pub(crate) fn debug_keys() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("MICROMUX_DEBUG_KEYS")
            .is_some_and(|value| !value.is_empty() && value != "0")
    })
}

/// Logs what micromux learned about the keyboard, at a level the default log keeps when
/// `MICROMUX_DEBUG_KEYS` is set.
fn log_keyboard(message: &str) {
    if debug_keys() {
        tracing::warn!(target: "micromux::keys", "{message}");
    } else {
        tracing::info!(target: "micromux::keys", "{message}");
    }
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
        crossterm::event::EnableMouseCapture.execute_winapi()
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
pub(crate) fn take_over(mouse: bool, keyboard_reporting: bool) -> io::Result<()> {
    let mut stdout = io::stdout();
    // crossterm counts raw mode as on even when a shell turned it off during a suspend.
    if RAW.swap(true, Ordering::SeqCst) {
        disable_raw_mode()?;
    }
    enable_raw_mode()?;
    // Asked before anything else, so the reader is not running yet.
    let keyboard = keyboard();
    SCREEN.store(true, Ordering::SeqCst);
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    if keyboard_reporting {
        ask_for_keyboard_on(&mut stdout, keyboard)?;
    }
    if mouse {
        MOUSE.store(true, Ordering::SeqCst);
        execute!(stdout, EnableButtonMouseCapture)?;
    }
    Ok(())
}

/// Turns the keyboard reporting micromux asks for on or off while the TUI shows.
///
/// PTY input mode turns it off, so the terminal sends a service's keys as it would without
/// micromux, Ctrl chords on any keyboard layout included; turning it on again while it is on
/// sends nothing.
pub(crate) fn set_keyboard_reporting(enabled: bool) -> io::Result<()> {
    set_keyboard_reporting_on(&mut io::stdout(), enabled, keyboard())
}

fn set_keyboard_reporting_on(
    out: &mut impl Write,
    enabled: bool,
    keyboard: Keyboard,
) -> io::Result<()> {
    if !enabled {
        return reset_keyboard_on(out);
    }
    if KEYBOARD.load(Ordering::SeqCst) != Keyboard::Legacy as u8 {
        return Ok(());
    }
    ask_for_keyboard_on(out, keyboard)
}

/// Takes back the keyboard reporting micromux asked for, once: pops the kitty flags or resets
/// modifyOtherKeys.
fn reset_keyboard_on(out: &mut impl Write) -> io::Result<()> {
    match KEYBOARD.swap(Keyboard::Legacy as u8, Ordering::SeqCst) {
        kitty if kitty == Keyboard::Kitty as u8 => write_ansi(out, PopKeyboardEnhancementFlags),
        other if other == Keyboard::ModifyOtherKeys as u8 => {
            out.write_all(MODIFY_OTHER_KEYS_RESET)?;
            out.flush()
        }
        _ => Ok(()),
    }
}

/// Asks the terminal to report keys as `keyboard` says, so the single reset in [`give_back`]
/// takes it back.
///
/// The kitty flags go onto a stack, so they are pushed once at most however often the terminal
/// is taken again. modifyOtherKeys is a plain setting, so it is asked for again every time, in
/// case a shell reset it while micromux was stopped.
fn ask_for_keyboard_on(out: &mut impl Write, keyboard: Keyboard) -> io::Result<()> {
    // The main and alternate screens keep separate kitty flag stacks, so the flags only go onto
    // the alternate screen's, and come off before leaving it.
    if keyboard == Keyboard::Legacy || !SCREEN.load(Ordering::SeqCst) {
        return Ok(());
    }
    let asked = KEYBOARD.swap(keyboard as u8, Ordering::SeqCst) == keyboard as u8;
    if asked && keyboard == Keyboard::Kitty {
        return Ok(());
    }
    match keyboard {
        Keyboard::Kitty => {
            log_keyboard(&format!("pushed kitty keyboard flags {KEYBOARD_FLAGS:?}"));
            write_ansi(out, PushKeyboardEnhancementFlags(KEYBOARD_FLAGS))
        }
        Keyboard::ModifyOtherKeys => {
            if !asked {
                log_keyboard("asked for modifyOtherKeys at level 2");
            }
            out.write_all(MODIFY_OTHER_KEYS_ON)?;
            out.flush()
        }
        Keyboard::Legacy => Ok(()),
    }
}

/// Writes `command`'s escape sequence to `out`.
///
/// crossterm would hand the kitty flags to the Windows console API, which has no such thing, on a
/// console without escape sequences; micromux only pushes them where the terminal answered that
/// it has the protocol, so the sequence is what it needs.
fn write_ansi(out: &mut impl Write, command: impl crossterm::Command) -> io::Result<()> {
    let mut ansi = String::new();
    command.write_ansi(&mut ansi).map_err(io::Error::other)?;
    out.write_all(ansi.as_bytes())?;
    out.flush()
}

/// Which keyboard reporting to ask the terminal for, decided once: the kitty protocol where the
/// terminal has it, else modifyOtherKeys where tmux passes it on as CSI u, else none.
fn keyboard() -> Keyboard {
    static CHOSEN: OnceLock<Keyboard> = OnceLock::new();
    *CHOSEN.get_or_init(|| {
        if has_keyboard_protocol() {
            Keyboard::Kitty
        } else if tmux_reports_keys_as_csi_u() {
            Keyboard::ModifyOtherKeys
        } else {
            Keyboard::Legacy
        }
    })
}

/// Whether micromux runs in a tmux pane set to report extended keys as CSI u, the form
/// crossterm reads.
///
/// tmux does not speak the kitty protocol, but with `extended-keys` on it reports the keys a pane
/// asks for with modifyOtherKeys, Ctrl+Shift+C apart from Ctrl-C. Its default form, xterm's
/// `CSI 27;…~`, is one crossterm cannot read, and xterm itself only sends that form, so
/// modifyOtherKeys is asked for nowhere else. Every program started from a pane inherits `TMUX`,
/// a terminal window included, so tmux must name micromux's own terminal as the pane's.
fn tmux_reports_keys_as_csi_u() -> bool {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// How long tmux may take to answer before micromux starts without it.
    const ANSWER_WITHIN: Duration = Duration::from_millis(500);

    let (Some(_), Some(pane)) = (
        std::env::var_os("TMUX").filter(|value| !value.is_empty()),
        std::env::var_os("TMUX_PANE").filter(|value| !value.is_empty()),
    ) else {
        return false;
    };
    let Some(own_tty) = crate::platform::own_tty() else {
        return false;
    };
    let Ok(mut child) = Command::new("tmux")
        .arg("display-message")
        .arg("-p")
        .arg("-t")
        .arg(pane)
        .arg("#{pane_tty}\t#{extended-keys}\t#{extended-keys-format}")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + ANSWER_WITHIN;
    let answered = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                child.kill().ok();
                child.wait().ok();
                break false;
            }
        }
    };
    let mut answer = String::new();
    if answered && let Some(mut stdout) = child.stdout.take() {
        std::io::Read::read_to_string(&mut stdout, &mut answer).ok();
    }
    let mut fields = answer.trim_end().split('\t');
    let (pane_tty, extended_keys, format) = (fields.next(), fields.next(), fields.next());
    log_keyboard(&format!(
        "tmux says its pane's terminal is {pane_tty:?}, micromux's {own_tty:?}, with \
         extended-keys {extended_keys:?} in format {format:?}"
    ));
    pane_tty == Some(own_tty.as_str())
        && matches!(extended_keys, Some("on" | "always"))
        && format == Some("csi-u")
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
    // Off the alternate screen first, as far as asking for keyboard reporting goes, so the event
    // loop cannot ask for it again in between, as when a second interrupt gives the terminal back
    // from another thread.
    let screen = SCREEN.swap(false, Ordering::SeqCst);
    step(reset_keyboard_on(out));
    if MOUSE.swap(false, Ordering::SeqCst) {
        step(execute!(out, DisableMouseCapture));
    }
    if screen {
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
pub(crate) fn stop_process_group() -> io::Result<()> {
    give_back()?;
    crate::platform::stop_process_group()
}

#[cfg(test)]
mod tests {
    use super::{KEYBOARD, Keyboard, SCREEN, ask_for_keyboard_on, set_keyboard_reporting_on};
    use color_eyre::eyre;
    use similar_asserts::assert_eq;
    use std::sync::atomic::Ordering;

    const KEYBOARD_POP: &str = "\x1b[<1u";
    const KEYBOARD_PUSH: &str = "\x1b[>5u";
    const MODIFY_OTHER_KEYS_ON: &str = "\x1b[>4;2m";
    const MODIFY_OTHER_KEYS_RESET: &str = "\x1b[>4m";

    fn written(out: &[u8]) -> String {
        String::from_utf8_lossy(out).into_owned()
    }

    /// PTY input mode turns the keyboard reporting off and leaving it turns it on again, each once
    /// however often the event loop asks; this needs no console, so it runs on Windows too.
    fn reporting_goes_off_and_on_with_pty_input_mode() -> eyre::Result<()> {
        for (keyboard, on, off) in [
            (Keyboard::Kitty, KEYBOARD_PUSH, KEYBOARD_POP),
            (
                Keyboard::ModifyOtherKeys,
                MODIFY_OTHER_KEYS_ON,
                MODIFY_OTHER_KEYS_RESET,
            ),
        ] {
            SCREEN.store(true, Ordering::SeqCst);
            KEYBOARD.store(Keyboard::Legacy as u8, Ordering::SeqCst);
            let mut out = Vec::new();
            for enabled in [true, true, false, false, true, true] {
                set_keyboard_reporting_on(&mut out, enabled, keyboard)?;
            }
            assert_eq!(written(&out), format!("{on}{off}{on}"), "{keyboard:?}");
        }
        // A terminal without either has nothing to turn off or on.
        KEYBOARD.store(Keyboard::Legacy as u8, Ordering::SeqCst);
        let mut out = Vec::new();
        for enabled in [true, false, true] {
            set_keyboard_reporting_on(&mut out, enabled, Keyboard::Legacy)?;
        }
        assert!(out.is_empty(), "{:?}", written(&out));
        SCREEN.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// The kitty flags go onto their stack once however often the terminal is taken, while
    /// modifyOtherKeys, a plain setting, is asked for again each time; this needs no console, so
    /// it runs on Windows too.
    #[cfg(windows)]
    #[test]
    fn keyboard_reporting_is_asked_for_once_per_kind() -> eyre::Result<()> {
        SCREEN.store(true, Ordering::SeqCst);
        let mut out = Vec::new();
        ask_for_keyboard_on(&mut out, Keyboard::Kitty)?;
        ask_for_keyboard_on(&mut out, Keyboard::Kitty)?;
        assert_eq!(written(&out), KEYBOARD_PUSH);

        KEYBOARD.store(Keyboard::Legacy as u8, Ordering::SeqCst);
        let mut out = Vec::new();
        ask_for_keyboard_on(&mut out, Keyboard::ModifyOtherKeys)?;
        ask_for_keyboard_on(&mut out, Keyboard::ModifyOtherKeys)?;
        ask_for_keyboard_on(&mut out, Keyboard::Legacy)?;
        assert_eq!(
            written(&out),
            format!("{MODIFY_OTHER_KEYS_ON}{MODIFY_OTHER_KEYS_ON}")
        );
        reporting_goes_off_and_on_with_pty_input_mode()
    }

    /// Giving the terminal back with every mode it took over, where the mouse and screen modes are
    /// escape sequences.
    ///
    /// On Windows crossterm sends those modes through the console API, which needs a console a
    /// test does not have, so `keyboard_reporting_is_asked_for_once_per_kind` covers the keyboard
    /// part there instead.
    /// The tests share the process-wide mode flags, so each platform runs them as one sequence.
    #[cfg(unix)]
    mod escape_sequences {
        use super::{
            KEYBOARD, KEYBOARD_POP, KEYBOARD_PUSH, Keyboard, MODIFY_OTHER_KEYS_ON,
            MODIFY_OTHER_KEYS_RESET, SCREEN, ask_for_keyboard_on,
            reporting_goes_off_and_on_with_pty_input_mode, written,
        };
        use crate::terminal::{MOUSE, RAW, give_back_on, set_mouse_capture_on};
        use color_eyre::eyre;
        use similar_asserts::assert_eq;
        use std::sync::atomic::Ordering;

        const MOUSE_OFF: &str = "\x1b[?1000l";
        const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
        const LEAVE_SCREEN: &str = "\x1b[?1049l";

        #[test]
        fn the_terminal_comes_back_exactly_once_with_whatever_was_on() -> eyre::Result<()> {
            // Giving back a terminal with every mode on pops the keyboard flags while still on the
            // alternate screen, whose stack they are on, releases the mouse, leaves the alternate
            // screen and turns raw mode off.
            RAW.store(true, Ordering::SeqCst);
            SCREEN.store(true, Ordering::SeqCst);
            KEYBOARD.store(Keyboard::Kitty as u8, Ordering::SeqCst);
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

            // Taking the terminal again without having given it back, as after a continue signal,
            // pushes the kitty flags only once, onto their stack, but asks for modifyOtherKeys, a
            // plain setting a shell may have reset meanwhile, again; either way the one reset on
            // the way out takes it back, before leaving the alternate screen.
            let leave = "\x1b[?2004l\x1b[?1049l\x1b[?25h";
            for (keyboard, expected) in [
                (
                    Keyboard::Kitty,
                    format!("{KEYBOARD_PUSH}{KEYBOARD_POP}{leave}"),
                ),
                (
                    Keyboard::ModifyOtherKeys,
                    format!(
                        "{MODIFY_OTHER_KEYS_ON}{MODIFY_OTHER_KEYS_ON}{MODIFY_OTHER_KEYS_RESET}{leave}"
                    ),
                ),
            ] {
                SCREEN.store(true, Ordering::SeqCst);
                let mut out = Vec::new();
                ask_for_keyboard_on(&mut out, keyboard)?;
                ask_for_keyboard_on(&mut out, keyboard)?;
                give_back_on(&mut out, || Ok(()))?;
                assert_eq!(written(&out), expected, "{keyboard:?}");
            }

            // Legacy reporting, or a terminal off the alternate screen, gets nothing.
            let mut out = Vec::new();
            SCREEN.store(true, Ordering::SeqCst);
            ask_for_keyboard_on(&mut out, Keyboard::Legacy)?;
            SCREEN.store(false, Ordering::SeqCst);
            ask_for_keyboard_on(&mut out, Keyboard::Kitty)?;
            assert!(out.is_empty(), "{:?}", written(&out));
            assert_eq!(KEYBOARD.load(Ordering::SeqCst), Keyboard::Legacy as u8);

            reporting_goes_off_and_on_with_pty_input_mode()
        }
    }
}
