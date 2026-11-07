//! Putting copied text on the system clipboard where the terminal cannot.
//!
//! The TUI always asks the terminal to copy through OSC 52, which also works over ssh.
//! Some terminals ignore that request, such as macOS Terminal and the VTE-based ones like GNOME
//! Terminal, so on a local session the text also goes to the platform's clipboard command.

use std::io::Write as _;
use std::process::{Command, Stdio};

/// Clipboard commands to try in order, each taking the text on standard input.
fn commands() -> Vec<(&'static str, &'static [&'static str])> {
    if cfg!(target_os = "macos") {
        vec![("pbcopy", &[])]
    } else if cfg!(windows) {
        // Windows Terminal supports OSC 52, and `clip` would garble text outside the console's
        // code page.
        Vec::new()
    } else {
        let mut commands = Vec::new();
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            commands.push(("wl-copy", &[] as &[&str]));
        }
        if std::env::var_os("DISPLAY").is_some() {
            commands.push(("xclip", &["-selection", "clipboard"]));
            commands.push(("xsel", &["--clipboard", "--input"]));
        }
        commands
    }
}

/// Starts `command` in a process group of its own, so Ctrl-Z, which stops micromux's group,
/// does not also stop what it started, such as a clipboard daemon that `wl-copy` or `xclip`
/// leaves serving the copied text, or a browser.
pub(crate) fn detach(command: &mut Command) {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    #[cfg(not(unix))]
    let _ = command;
}

/// Whether the TUI runs over ssh, where a local clipboard command would copy, and a browser
/// open, on the remote host.
pub(crate) fn over_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
}

/// Hands `text` to the first clipboard command that runs, without waiting for it, unless the
/// TUI runs over ssh; a missing or failing command is skipped silently, since the terminal may
/// have copied the text already.
pub(crate) fn copy_locally(text: String) {
    if over_ssh() {
        return;
    }
    for (program, args) in commands() {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        detach(&mut command);
        let spawned = command.spawn();
        let Ok(mut child) = spawned else {
            continue;
        };
        std::thread::spawn(move || {
            if let Some(mut stdin) = child.stdin.take() {
                // A command that stopped reading copied nothing more that could be saved.
                let _ = stdin.write_all(text.as_bytes());
            }
            // Reap the command once it exits, so it does not linger as a zombie.
            let _ = child.wait();
        });
        return;
    }
}
