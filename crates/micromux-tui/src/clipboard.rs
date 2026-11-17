//! Putting copied text on the system clipboard where the terminal cannot.
//!
//! The TUI always asks the terminal to copy through OSC 52, which also works over ssh.
//! Some terminals ignore that request, such as macOS Terminal and the VTE-based ones like GNOME
//! Terminal, so on a local session the text also goes to the platform's clipboard command.

use std::io::Write as _;
use std::process::{Command, Stdio};

/// A clipboard command and its arguments, which takes the text on standard input.
type Candidate = (&'static str, &'static [&'static str]);

/// Clipboard commands to try in order, each taking the text on standard input.
fn commands() -> Vec<Candidate> {
    if cfg!(target_os = "macos") {
        vec![("pbcopy", &[])]
    } else if cfg!(windows) {
        // Windows Terminal supports OSC 52, and `clip` would garble text outside the console's
        // code page.
        Vec::new()
    } else {
        let has = |name| std::env::var_os(name).is_some();
        let gnome = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktops| gnome(&desktops));
        unix_commands(has("WAYLAND_DISPLAY"), has("DISPLAY"), gnome)
    }
}

/// Clipboard commands to try in order on a Unix desktop with the given displays.
fn unix_commands(wayland: bool, x11: bool, gnome: bool) -> Vec<Candidate> {
    let wayland: &[Candidate] = if wayland { &[("wl-copy", &[])] } else { &[] };
    let x11: &[Candidate] = if x11 {
        &[
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    } else {
        &[]
    };
    // GNOME offers no Wayland clipboard to a program without a window, so `wl-copy` maps one for
    // a moment there, which flashes in the dock.
    // The X11 tools copy through XWayland, which passes the text on to Wayland, with a window
    // that never shows, so they go first and `wl-copy` stays the fallback.
    if gnome {
        [x11, wayland].concat()
    } else {
        [wayland, x11].concat()
    }
}

/// Whether `XDG_CURRENT_DESKTOP`, a colon-separated list such as `ubuntu:GNOME`, names GNOME.
fn gnome(desktops: &str) -> bool {
    desktops
        .split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case("gnome"))
}

/// Whether the TUI runs over ssh, where a local clipboard command would copy, and a browser
/// open, on the remote host.
pub(crate) fn over_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
}

/// Hands `text` to the platform's clipboard commands on a background thread, unless the TUI runs
/// over ssh; a missing or failing command is skipped silently, since the terminal may have copied
/// the text already.
pub(crate) fn copy_locally(text: String) {
    if over_ssh() {
        return;
    }
    let candidates = commands();
    if candidates.is_empty() {
        return;
    }
    std::thread::spawn(move || copy_with(&candidates, &text));
}

/// Hands `text` to each of `candidates` in turn until one exits successfully, and returns the
/// index of that one.
///
/// A command that starts can still fail, such as `xclip` with a `DISPLAY` that a tmux session
/// inherited from a login that has since ended, so the next one gets its turn too.
/// The clipboard commands fork into the background once they own the selection, so waiting for
/// each to exit takes only as long as the handover.
fn copy_with(candidates: &[Candidate], text: &str) -> Option<usize> {
    candidates.iter().position(|&(program, args)| {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::platform::detach(&mut command);
        let Ok(mut child) = command.spawn() else {
            return false;
        };
        if let Some(mut stdin) = child.stdin.take() {
            // A command that stops reading fails, and its exit status says so.
            let _ = stdin.write_all(text.as_bytes());
        }
        // Waiting also reaps the command, so it does not linger as a zombie.
        child.wait().is_ok_and(|status| status.success())
    })
}

#[cfg(test)]
mod tests {
    use super::{gnome, unix_commands};
    use similar_asserts::assert_eq;

    fn programs(wayland: bool, x11: bool, gnome: bool) -> Vec<&'static str> {
        unix_commands(wayland, x11, gnome)
            .into_iter()
            .map(|(program, _)| program)
            .collect()
    }

    /// On GNOME the X11 tools go before `wl-copy`, which would flash a window in the dock, and
    /// elsewhere `wl-copy` stays first.
    #[test]
    fn gnome_prefers_the_x11_tools() {
        assert_eq!(programs(true, true, true), ["xclip", "xsel", "wl-copy"]);
        assert_eq!(programs(true, true, false), ["wl-copy", "xclip", "xsel"]);
    }

    /// A display that is not set contributes no commands, on GNOME or elsewhere.
    #[test]
    fn missing_displays_drop_their_commands() {
        for on_gnome in [false, true] {
            assert_eq!(programs(true, false, on_gnome), ["wl-copy"]);
            assert_eq!(programs(false, true, on_gnome), ["xclip", "xsel"]);
            assert_eq!(programs(false, false, on_gnome), Vec::<&str>::new());
        }
    }

    /// Any entry of the colon-separated list names GNOME, in any case, but only as a whole entry.
    #[test]
    fn detects_gnome_in_the_desktop_list() {
        assert!(gnome("GNOME"));
        assert!(gnome("ubuntu:GNOME"));
        assert!(gnome("gnome:other"));
        assert!(!gnome("KDE"));
        assert!(!gnome("GNOME-Classic"));
        assert!(!gnome(""));
    }

    /// A command that is missing, or that starts but fails, passes the text on to the next one,
    /// as `wl-copy` must still copy when an `xclip` before it cannot reach its X display.
    #[cfg(unix)]
    #[test]
    fn a_failing_command_falls_back_to_the_next() {
        use super::copy_with;

        // The last command succeeds only when it reads exactly the copied text.
        let candidates = [
            ("micromux-no-such-clipboard-command", &[] as &[&str]),
            ("sh", &["-c", "cat >/dev/null; exit 1"]),
            ("sh", &["-c", r#"test "$(cat)" = 'copied text'"#]),
        ];
        assert_eq!(copy_with(&candidates, "copied text"), Some(2));

        // The first command that succeeds ends the search.
        let candidates = [("true", &[] as &[&str]), ("false", &[])];
        assert_eq!(copy_with(&candidates, "copied text"), Some(0));

        // When every command fails, nothing copies.
        let candidates = [("false", &[] as &[&str]), ("false", &[])];
        assert_eq!(copy_with(&candidates, "copied text"), None);
    }
}
