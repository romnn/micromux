//! The terminal modes the TUI takes and gives back, driven through the real binary in a
//! pseudo-terminal.
#![cfg(unix)]

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use color_eyre::eyre;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use similar_asserts::assert_eq;
use vt100::MouseProtocolMode;

const MICROMUX: &str = env!("CARGO_BIN_EXE_micromux");

/// How long the TUI may take to reach a state the test waits for.
const WAIT: Duration = Duration::from_secs(30);

/// The sequence that switches mouse reporting on.
const MOUSE_ON: &[u8] = b"\x1b[?1000h";

/// The sequences that push and pop micromux's kitty keyboard flags.
const KEYBOARD_PUSH: &[u8] = b"\x1b[>5u";
const KEYBOARD_POP: &[u8] = b"\x1b[<1u";

/// The terminal's queries for its kitty keyboard flags and its primary device attributes.
const KEYBOARD_QUERY: &[u8] = b"\x1b[?u";
const ATTRIBUTES_QUERY: &[u8] = b"\x1b[c";

/// The OSC 52 copy of `hello` to the clipboard.
const HELLO_COPIED: &[u8] = b"\x1b]52;c;aGVsbG8=";

const CONFIG: &str = "version: 1\nservices:\n  app:\n    command: [\"sh\", \"-c\", \"echo hello world; sleep 30\"]\n";

/// A program running in a pseudo-terminal, with the screen it drew and everything it wrote.
struct Terminal {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    writer: Box<dyn std::io::Write + Send>,
    output: Receiver<Vec<u8>>,
    parser: vt100::Parser,
    written: Vec<u8>,
    /// How much of `written` was searched for queries to answer.
    answered: usize,
    /// Whether to answer as a terminal with the kitty keyboard protocol.
    keyboard_protocol: bool,
    started: Instant,
    /// The process groups killed when the test ends, the program's own first.
    groups: Vec<i32>,
}

impl Terminal {
    fn spawn(command: CommandBuilder) -> eyre::Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| eyre::eyre!("{err}"))?;
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|err| eyre::eyre!("{err}"))?;
        drop(pair.slave);
        // The program leads a session and process group of its own.
        let groups = child
            .process_id()
            .and_then(|pid| i32::try_from(pid).ok())
            .into_iter()
            .collect();
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| eyre::eyre!("{err}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|err| eyre::eyre!("{err}"))?;
        let (sender, output) = mpsc::channel();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(read) = reader.read(&mut chunk) {
                if read == 0
                    || sender
                        .send(chunk.get(..read).unwrap_or_default().to_vec())
                        .is_err()
                {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            writer,
            output,
            parser: vt100::Parser::new(30, 120, 0),
            written: Vec::new(),
            answered: 0,
            keyboard_protocol: false,
            started: Instant::now(),
            groups,
        })
    }

    /// Feeds the program's output to the screen until `done` holds for it.
    fn wait_until(
        &mut self,
        what: &str,
        done: impl Fn(&vt100::Screen, &[u8]) -> bool,
    ) -> eyre::Result<()> {
        let deadline = Instant::now() + WAIT;
        while !done(self.parser.screen(), &self.written) {
            if Instant::now() >= deadline {
                eyre::bail!(self.failure(&format!("timed out waiting for {what}")));
            }
            match self.output.recv_timeout(Duration::from_millis(50)) {
                Ok(chunk) => {
                    self.parser.process(&chunk);
                    self.written.extend_from_slice(&chunk);
                    self.answer_queries()?;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    if !done(self.parser.screen(), &self.written) {
                        eyre::bail!(
                            self.failure(&format!("the program closed the terminal before {what}"))
                        );
                    }
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Says `what` went wrong, with the screen and the last bytes the program wrote, escaped.
    fn failure(&self, what: &str) -> String {
        const TAIL: usize = 4096;
        let tail = self
            .written
            .get(self.written.len().saturating_sub(TAIL)..)
            .unwrap_or_default();
        format!(
            "{what} after {:?}; the program wrote {} bytes, ending in:\n{}\nthe screen shows:\n{}",
            self.started.elapsed(),
            self.written.len(),
            tail.escape_ascii(),
            self.parser.screen().contents()
        )
    }

    /// Answers the queries written since the last call as a terminal does, which vt100 does not:
    /// the keyboard flags only with the kitty protocol, the device attributes always.
    fn answer_queries(&mut self) -> eyre::Result<()> {
        loop {
            let rest = self.written.get(self.answered..).unwrap_or_default();
            let next = [KEYBOARD_QUERY, ATTRIBUTES_QUERY]
                .into_iter()
                .filter_map(|query| position(rest, query).map(|at| (at, query)))
                .min_by_key(|&(at, _)| at);
            let Some((at, query)) = next else {
                return Ok(());
            };
            self.answered += at + query.len();
            if query == ATTRIBUTES_QUERY {
                self.send(b"\x1b[?62c")?;
            } else if self.keyboard_protocol {
                self.send(b"\x1b[?0u")?;
            }
        }
    }

    /// How often `needle` occurs in what the program wrote.
    fn count(&self, needle: &[u8]) -> usize {
        self.written
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
    }

    fn send(&mut self, keys: &[u8]) -> eyre::Result<()> {
        self.writer.write_all(keys)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Waits for the TUI's first frame.
    fn wait_for_tui(&mut self) -> eyre::Result<()> {
        self.wait_until("the first frame", |screen, _| {
            screen.alternate_screen() && screen.contents().contains("Services")
        })
    }

    /// Quits the TUI with `q` and waits for the process to exit.
    fn quit(&mut self) -> eyre::Result<()> {
        self.send(b"q")?;
        self.wait_for_exit()
    }

    /// Waits for the process to exit and reads what it wrote on its way out.
    fn wait_for_exit(&mut self) -> eyre::Result<()> {
        let deadline = Instant::now() + WAIT;
        while self.child.try_wait()?.is_none() {
            eyre::ensure!(Instant::now() < deadline, "micromux did not exit");
            std::thread::sleep(Duration::from_millis(50));
        }
        // Read what the TUI wrote on its way out.
        self.wait_until("the end of the output", |_, _| false).ok();
        Ok(())
    }
}

impl Drop for Terminal {
    /// Kills every process group the test ran, however it ended, so no supervisor outlives it,
    /// not even one stopped as a job, which only `SIGKILL` reliably ends.
    fn drop(&mut self) {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;

        // Once the program is reaped its pid, and so its group's id, may name another group.
        let reaped = !matches!(self.child.try_wait(), Ok(None));
        for &group in self.groups.iter().skip(usize::from(reaped)) {
            // A group that is gone already, as after a clean exit, has nothing to signal.
            killpg(Pid::from_raw(group), Signal::SIGKILL).ok();
        }
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// The process group the wrapper printed as `job=<pid>`.
fn job_group(written: &[u8]) -> eyre::Result<i32> {
    let text = String::from_utf8_lossy(written);
    // Only a finished line holds the whole pid.
    let finished = text.rsplit_once('\n').map_or("", |(finished, _)| finished);
    // The shell may start the line with mode changes, and its echo of the command line names
    // `$$`, not a number.
    finished
        .lines()
        .filter_map(|line| line.split_once("job=").map(|(_, rest)| rest))
        .find_map(|rest| {
            let digits = rest.split(|c: char| !c.is_ascii_digit()).next()?;
            digits.parse().ok()
        })
        .ok_or_else(|| eyre::eyre!("the wrapper printed no job:\n{text}"))
}

/// Whether `needle` occurs in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    position(haystack, needle).is_some()
}

/// Where `needle` first occurs in `haystack`.
fn position(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A project in a fresh directory, with a home of its own so no user config leaks in.
struct Project {
    directory: tempfile::TempDir,
}

impl Project {
    fn new(config: &str) -> eyre::Result<Self> {
        let directory = tempfile::tempdir()?;
        std::fs::create_dir_all(directory.path().join("home"))?;
        std::fs::write(directory.path().join("micromux.yaml"), config)?;
        Ok(Self { directory })
    }

    fn home(&self) -> PathBuf {
        self.directory.path().join("home")
    }

    /// Writes the user config micromux reads under this project's home.
    fn write_user_config(&self, config: &str) -> eyre::Result<()> {
        let path = self.home().join(".config/micromux/config.yaml");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, config)?;
        Ok(())
    }

    /// `program` with `args`, run in the project with its home.
    fn command(&self, program: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> CommandBuilder {
        let mut command = CommandBuilder::new(program);
        command.args(args);
        command.cwd(self.directory.path());
        command.env("TERM", "xterm-256color");
        command.env("HOME", self.home());
        // State micromux shares per user, such as the port registry, stays in the project.
        command.env("MICROMUX_STATE_DIR", self.directory.path().join("state"));
        command.env_remove("XDG_CONFIG_HOME");
        command.env_remove("MICROMUX_MOUSE");
        command.env_remove("MICROMUX_COPY_ON_SELECT");
        // Outside tmux, unless a test puts micromux in a tmux of its own.
        command.env_remove("TMUX");
        command.env_remove("TMUX_PANE");
        // A copy would otherwise also reach the desktop's clipboard, overwriting the user's.
        command.env_remove("DISPLAY");
        command.env_remove("WAYLAND_DISPLAY");
        command
    }

    fn micromux(&self, args: &[&str]) -> eyre::Result<Terminal> {
        let mut all = vec!["--no-control"];
        all.extend_from_slice(args);
        Terminal::spawn(self.command(MICROMUX, &all))
    }

    /// micromux as if inside a tmux pane, whose `tmux` command says it reports extended keys in
    /// `format`, and names micromux's own terminal as the pane's or, unless `own_pane`, another
    /// one, as for a terminal window started from a pane.
    fn micromux_in_tmux(&self, format: &str, own_pane: bool) -> eyre::Result<Terminal> {
        use std::os::unix::fs::PermissionsExt as _;

        let bin = self.directory.path().join("bin");
        std::fs::create_dir_all(&bin)?;
        let tmux = bin.join("tmux");
        std::fs::write(
            &tmux,
            format!(
                "#!/bin/sh\n\
                 case \"$*\" in *display-message*) ;; *) exit 1 ;; esac\n\
                 tty=/dev/$(ps -o tty= -p $PPID | tr -d ' ')\n\
                 [ {own_pane} = true ] || tty=/dev/pts/999999\n\
                 printf '%s\\ton\\t{format}\\n' \"$tty\"\n"
            ),
        )?;
        std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755))?;
        let mut command = self.command(MICROMUX, &["--no-control"]);
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&path));
        command.env("PATH", std::env::join_paths(paths)?);
        command.env("TMUX", "/tmp/tmux-test/default,1,0");
        command.env("TMUX_PANE", "%1");
        Terminal::spawn(command)
    }
}

/// The zero-based column and row where `needle` is drawn on `screen`.
fn find(screen: &vt100::Screen, needle: &str) -> eyre::Result<(u16, u16)> {
    let (_, columns) = screen.size();
    for (row, text) in screen.rows(0, columns).enumerate() {
        if let Some(byte) = text.find(needle) {
            let column = text.get(..byte).unwrap_or_default().chars().count();
            return Ok((u16::try_from(column)?, u16::try_from(row)?));
        }
    }
    eyre::bail!("{needle:?} is not on screen:\n{}", screen.contents())
}

/// The SGR mouse report for button `code` at a zero-based cell, pressed or released.
fn mouse_report(code: u8, (column, row): (u16, u16), pressed: bool) -> Vec<u8> {
    let end = if pressed { 'M' } else { 'm' };
    format!("\x1b[<{code};{};{}{end}", column + 1, row + 1).into_bytes()
}

/// Waits for the service's output, focuses the logs with a click on the focus hint and selects
/// `hello`.
fn focus_the_logs_and_select_hello(tui: &mut Terminal) -> eyre::Result<()> {
    tui.wait_for_tui()?;
    // A frame can arrive in pieces, so its footer may follow the service's output.
    tui.wait_until("the service's output and the focus hint", |screen, _| {
        screen.contents().contains("hello world") && screen.contents().contains("Tab Focus")
    })?;
    let hint = find(tui.parser.screen(), "Tab Focus")?;
    tui.send(&mouse_report(0, hint, true))?;
    tui.send(&mouse_report(0, hint, false))?;
    tui.wait_until("the logs to take the focus", |screen, _| {
        screen.contents().contains("Focus:LOGS")
    })?;
    select_hello(tui)
}

/// Selects `hello` in the logs by dragging over it, which copies nothing yet.
fn select_hello(tui: &mut Terminal) -> eyre::Result<()> {
    let (column, row) = find(tui.parser.screen(), "hello")?;
    tui.send(&mouse_report(0, (column, row), true))?;
    tui.send(&mouse_report(32, (column + 4, row), true))?;
    tui.send(&mouse_report(0, (column + 4, row), false))?;
    // Input is handled in order, so once `H` shows, the release before it was handled too.
    tui.send(b"H")?;
    tui.wait_until("the release and the copy key in the footer", |screen, _| {
        screen.contents().contains("y Copy") && screen.contents().contains("Health:ON")
    })?;
    assert!(!contains(&tui.written, b"\x1b]52;"));
    Ok(())
}

/// Waits until `hello` was copied to the clipboard `times` times in all.
fn wait_for_copies(tui: &mut Terminal, what: &str, times: usize) -> eyre::Result<()> {
    tui.wait_until(what, |_, written| {
        written
            .windows(HELLO_COPIED.len())
            .filter(|window| *window == HELLO_COPIED)
            .count()
            == times
    })
}

/// Waits for the footer to say that `q` quits, and for it to stop saying so, and checks that
/// micromux still runs.
fn wait_out_the_quit_hint(tui: &mut Terminal) -> eyre::Result<()> {
    tui.wait_until("the quit hint", |screen, _| {
        screen.contents().contains("press q to quit")
    })?;
    tui.wait_until("the quit hint to go", |screen, _| {
        !screen.contents().contains("press q to quit")
    })?;
    eyre::ensure!(tui.child.try_wait()?.is_none(), "a Ctrl-C stopped micromux");
    Ok(())
}

/// Selects `hello` in the logs, copies it with `y` and with Ctrl+Shift+C in both forms a
/// kitty-protocol terminal reports it, and checks that Ctrl-C copies nothing and stops nothing.
fn copy_hello_and_survive_a_ctrl_c(tui: &mut Terminal) -> eyre::Result<()> {
    // Dragging over a word of the logs selects it, and only `y` copies it to the clipboard
    // through OSC 52.
    select_hello(tui)?;
    tui.send(b"y")?;
    tui.wait_until("the copy", |_, written| contains(written, HELLO_COPIED))?;
    // One copy to the clipboard and one to the primary selection.
    assert_eq!(tui.count(b"\x1b]52;"), 2);

    // Ctrl+Shift+C copies the same way, both as a terminal reports it with the shifted key and
    // as one that reports Shift instead.
    tui.send(b"\x1b[99:67;6u")?;
    wait_for_copies(tui, "the copy with Ctrl+Shift+C as C", 2)?;
    tui.send(b"\x1b[99;6u")?;
    wait_for_copies(tui, "the copy with Ctrl+Shift+C as c with Shift", 3)?;

    // Ctrl-C never copies, even with text selected, and never quits, however often it comes.
    tui.send(b"\x1b[99;5u")?;
    tui.send(b"\x1b[99;5u")?;
    tui.send(b"\x03")?;
    wait_out_the_quit_hint(tui)?;
    assert_eq!(tui.count(HELLO_COPIED), 3);
    Ok(())
}

/// Capture and the kitty keyboard flags come back off whichever way the TUI lets go of the
/// terminal: the `m` key for capture, a suspend, and quitting.
#[test]
fn mouse_capture_is_released_by_the_toggle_a_suspend_and_quitting() -> eyre::Result<()> {
    let project = Project::new(CONFIG)?;
    let mut tui = project.micromux(&[])?;
    tui.keyboard_protocol = true;
    tui.wait_for_tui()?;
    // The flags go onto the alternate screen's stack.
    let entered = position(&tui.written, b"\x1b[?1049h");
    let pushed = position(&tui.written, KEYBOARD_PUSH);
    assert!(
        entered.is_some() && pushed.is_some() && entered < pushed,
        "{:?}",
        String::from_utf8_lossy(&tui.written)
    );
    tui.wait_until("mouse capture", |screen, _| {
        screen.mouse_protocol_mode() == MouseProtocolMode::ButtonMotion
    })?;
    // A frame can arrive in pieces, so its footer may follow the service's output.
    tui.wait_until("the service's output and the focus hint", |screen, _| {
        screen.contents().contains("hello world") && screen.contents().contains("Tab Focus")
    })?;

    // A click on a hint runs its key, as the terminal reports it.
    let hint = find(tui.parser.screen(), "Tab Focus")?;
    tui.send(&mouse_report(0, hint, true))?;
    tui.send(&mouse_report(0, hint, false))?;
    tui.wait_until("the logs to take the focus", |screen, _| {
        screen.contents().contains("Focus:LOGS")
    })?;

    copy_hello_and_survive_a_ctrl_c(&mut tui)?;

    // PTY input mode pops the keyboard flags, so the terminal sends the service its keys as
    // without micromux, and Ctrl+], which every terminal sends as the byte 0x1d, leaves it and
    // pushes them again.
    tui.send(b"a")?;
    tui.wait_until("PTY input mode and the flags popped", |screen, written| {
        screen.contents().contains("Ctrl+] Exit") && contains(written, KEYBOARD_POP)
    })?;
    tui.send(b"\x1d")?;
    tui.wait_until(
        "PTY input mode to end and the flags back",
        |screen, written| {
            !screen.contents().contains("Ctrl+] Exit")
                && written
                    .windows(KEYBOARD_PUSH.len())
                    .filter(|window| *window == KEYBOARD_PUSH)
                    .count()
                    == 2
        },
    )?;

    // The toggle releases the mouse and takes it again.
    tui.send(b"m")?;
    tui.wait_until("capture to go off", |screen, _| {
        screen.mouse_protocol_mode() == MouseProtocolMode::None
            && screen.contents().contains("Mouse:OFF")
    })?;
    tui.send(b"m")?;
    tui.wait_until("capture to come back", |screen, _| {
        screen.mouse_protocol_mode() == MouseProtocolMode::ButtonMotion
            && screen.contents().contains("Mouse:ON")
    })?;

    // Ctrl-Z gives the terminal back before it stops, popping the keyboard flags off the
    // alternate screen before leaving it.
    // Nothing can continue a process in this orphaned process group, so the system ignores the
    // stop and the TUI takes the terminal right back.
    let before = tui.written.len();
    tui.send(b"\x1a")?;
    tui.wait_until(
        "the terminal to be given back and taken again",
        |screen, written| {
            let after = written.get(before..).unwrap_or_default();
            let released = after
                .windows(b"\x1b[?1049l".len())
                .position(|window| window == b"\x1b[?1049l");
            let popped = position(after, KEYBOARD_POP);
            released.is_some_and(|at| {
                let again = after.get(at..).unwrap_or_default();
                contains(again, MOUSE_ON) && contains(again, KEYBOARD_PUSH)
            }) && popped.is_some()
                && popped < released
                && contains(after, b"\x1b[?1000l")
                && screen.alternate_screen()
                && screen.mouse_protocol_mode() == MouseProtocolMode::ButtonMotion
                && screen.contents().contains("Services")
        },
    )?;

    // A continue signal without a stop, as `kill -CONT` sends, takes the terminal over afresh
    // without pushing the keyboard flags a second time.
    let pid = tui
        .child
        .process_id()
        .ok_or_else(|| eyre::eyre!("micromux has no process id"))?;
    let before = tui.written.len();
    let status = std::process::Command::new("kill")
        .args(["-CONT", &pid.to_string()])
        .status()?;
    eyre::ensure!(status.success(), "kill -CONT failed");
    tui.wait_until("the terminal to be taken over afresh", |_, written| {
        contains(written.get(before..).unwrap_or_default(), b"\x1b[2J")
    })?;

    // `q` quits, releasing the mouse, the keyboard flags and the alternate screen.
    let before = tui.written.len();
    tui.send(b"q")?;
    tui.wait_for_exit()?;
    let screen = tui.parser.screen();
    assert_eq!(screen.mouse_protocol_mode(), MouseProtocolMode::None);
    assert!(!screen.alternate_screen());
    let after = tui.written.get(before..).unwrap_or_default();
    let popped = position(after, KEYBOARD_POP);
    assert!(
        popped.is_some() && popped < position(after, b"\x1b[?1049l"),
        "{:?}",
        String::from_utf8_lossy(after)
    );
    // Every push came off again.
    assert_eq!(tui.count(KEYBOARD_PUSH), tui.count(KEYBOARD_POP));
    Ok(())
}

/// In a terminal without the kitty keyboard protocol, as inside tmux, which answers only the
/// device attributes query, micromux pushes no flags; Ctrl+Shift+C then arrives as the Ctrl-C
/// byte, which copies nothing and stops nothing, while Ctrl+Insert, which such terminals still
/// send apart, copies.
#[test]
fn without_the_keyboard_protocol_ctrl_shift_c_is_a_harmless_ctrl_c() -> eyre::Result<()> {
    let project = Project::new(CONFIG)?;
    let mut tui = project.micromux(&[])?;
    focus_the_logs_and_select_hello(&mut tui)?;

    tui.send(b"\x03")?;
    wait_out_the_quit_hint(&mut tui)?;
    assert!(!contains(&tui.written, b"\x1b]52;"));

    tui.send(b"\x1b[2;5~")?;
    wait_for_copies(&mut tui, "the copy with Ctrl+Insert", 1)?;
    tui.quit()?;
    assert!(!contains(&tui.written, b"\x1b[>"));
    Ok(())
}

/// Inside tmux, which has no kitty keyboard protocol but reports extended keys as CSI u where set
/// to, micromux asks for modifyOtherKeys, so Ctrl+Shift+C arrives apart from Ctrl-C and copies,
/// and resets it on the way out.
#[test]
fn inside_tmux_modify_other_keys_makes_ctrl_shift_c_copy() -> eyre::Result<()> {
    const ON: &[u8] = b"\x1b[>4;2m";
    const RESET: &[u8] = b"\x1b[>4m";

    let project = Project::new(CONFIG)?;
    let mut tui = project.micromux_in_tmux("csi-u", true)?;
    focus_the_logs_and_select_hello(&mut tui)?;
    assert_eq!(tui.count(ON), 1);
    assert!(!contains(&tui.written, KEYBOARD_PUSH));

    // Ctrl+Shift+C, as tmux reports it, copies; Ctrl-C, in either form, only says that `q`
    // quits.
    tui.send(b"\x1b[99;6u")?;
    wait_for_copies(&mut tui, "the copy with Ctrl+Shift+C", 1)?;
    tui.send(b"\x1b[99;5u")?;
    tui.send(b"\x03")?;
    wait_out_the_quit_hint(&mut tui)?;
    assert_eq!(tui.count(HELLO_COPIED), 1);

    // Quitting resets it before leaving the alternate screen.
    let before = tui.written.len();
    tui.quit()?;
    let after = tui.written.get(before..).unwrap_or_default();
    let reset = position(after, RESET);
    assert!(
        reset.is_some() && reset < position(after, b"\x1b[?1049l"),
        "{:?}",
        String::from_utf8_lossy(after)
    );
    assert_eq!(tui.count(RESET), 1);

    // tmux's default form for extended keys is one micromux cannot read, and a terminal started
    // from a pane is not tmux, so in neither micromux asks for anything.
    for (format, own_pane) in [("xterm", true), ("csi-u", false)] {
        let project = Project::new(CONFIG)?;
        let mut tui = project.micromux_in_tmux(format, own_pane)?;
        tui.wait_for_tui()?;
        tui.quit()?;
        assert!(!contains(&tui.written, b"\x1b[>4"), "{format} {own_pane}");
    }
    Ok(())
}

/// The flag beats the project config, which beats the user config, and "off" never sends the
/// terminal a mouse sequence at all.
///
/// The terminal here has no kitty keyboard protocol, so micromux never asks it for one.
#[test]
fn the_mouse_setting_follows_the_flag_then_the_project_then_the_user() -> eyre::Result<()> {
    let run = |project: &Project, args: &[&str]| -> eyre::Result<bool> {
        let mut tui = project.micromux(args)?;
        tui.wait_for_tui()?;
        tui.quit()?;
        assert!(!contains(&tui.written, b"\x1b[>"));
        assert!(!contains(&tui.written, KEYBOARD_POP));
        Ok(contains(&tui.written, MOUSE_ON))
    };

    // The user config turns it off, and nothing overrides it.
    let user_off = Project::new(CONFIG)?;
    user_off.write_user_config("ui:\n  mouse: false\n")?;
    assert!(!run(&user_off, &[])?);
    // The flag turns it back on.
    assert!(run(&user_off, &["--mouse", "on"])?);

    // The project config overrides the user config, and the flag overrides the project.
    let project_on = Project::new(&format!("ui:\n  mouse: true\n{CONFIG}"))?;
    project_on.write_user_config("ui:\n  mouse: false\n")?;
    assert!(run(&project_on, &[])?);
    assert!(!run(&project_on, &["--mouse", "off"])?);

    // Without any setting the mouse is captured.
    let unset = Project::new(CONFIG)?;
    assert!(run(&unset, &[])?);
    Ok(())
}

/// In a shell with job control, Ctrl-Z stops micromux with the terminal given back, and `fg`
/// brings the TUI back with the mouse captured and the keyboard flags pushed again.
///
/// micromux runs under a wrapper, as under `cargo run` or a task runner, which must stop with it
/// for the shell to see the job stop.
#[test]
fn ctrl_z_suspends_to_the_shell_and_fg_resumes() -> eyre::Result<()> {
    let bash = Path::new("/bin/bash");
    if !bash.exists() {
        return Ok(());
    }
    let project = Project::new(CONFIG)?;
    let mut command = project.command(bash, &["--norc", "--noprofile", "-i"]);
    command.env("PS1", "shell$ ");
    let mut shell = Terminal::spawn(command)?;
    shell.keyboard_protocol = true;
    shell.wait_until("the prompt", |screen, _| {
        screen.contents().contains("shell$")
    })?;
    // The wrapper says its job's process group, for the test to kill however it ends, and its
    // last line differs from the command line the shell echoes.
    shell.send(
        format!("sh -c 'echo job=$$; {MICROMUX} --no-control; printf \"wrapper%s\\n\" -done'\r")
            .as_bytes(),
    )?;
    shell.wait_until("the wrapper's job", |_, written| job_group(written).is_ok())?;
    let job = job_group(&shell.written)?;
    shell.groups.push(job);
    shell.wait_for_tui()?;
    shell.wait_until("mouse capture", |screen, _| {
        screen.mouse_protocol_mode() == MouseProtocolMode::ButtonMotion
    })?;

    // The shell reports the stopped job on its own screen, with the mouse released.
    shell.send(b"\x1a")?;
    shell.wait_until("the job to stop", |screen, _| {
        !screen.alternate_screen()
            && screen.mouse_protocol_mode() == MouseProtocolMode::None
            && screen.contents().contains("Stopped")
    })?;

    // `fg` continues it, and the TUI redraws with the mouse captured.
    shell.send(b"fg\r")?;
    shell.wait_until("the TUI to come back", |screen, _| {
        screen.alternate_screen()
            && screen.mouse_protocol_mode() == MouseProtocolMode::ButtonMotion
            && screen.contents().contains("Services")
    })?;

    shell.send(b"q")?;
    shell.wait_until("the wrapper to finish", |screen, _| {
        !screen.alternate_screen()
            && screen.mouse_protocol_mode() == MouseProtocolMode::None
            && screen.contents().contains("wrapper-done")
    })?;
    // The job is done, so its group's id may be reused from now on.
    shell.groups.retain(|&group| group != job);
    // However the wake after `fg` raced the suspend's own, the keyboard flags came off as often
    // as they went on.
    assert_eq!(shell.count(KEYBOARD_PUSH), 2);
    assert_eq!(shell.count(KEYBOARD_POP), 2);
    shell.send(b"exit\r")?;
    Ok(())
}
