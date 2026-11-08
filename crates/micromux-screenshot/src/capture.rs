//! Driving the real `micromux` in a pseudo-terminal until its screen settles.

use crate::Error;
use crate::scenario::Scenario;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read as _, Write};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Run `micromux` in a PTY against `example_dir` with `home` as its home directory, drive the
/// scenario, and return the settled screen.
pub(crate) fn capture(
    micromux: &Path,
    home: &Path,
    state_dir: &Path,
    example_dir: &Path,
    scenario: &Scenario,
) -> Result<vt100::Screen, Error> {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: scenario.rows.get(),
            cols: scenario.cols.get(),
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| Error::operation("failed to open pty", err))?;

    let mut cmd = CommandBuilder::new(micromux);
    cmd.cwd(example_dir);
    cmd.env("TERM", "xterm-256color");
    // The TUI shows log timestamps in local time, so pin the zone to render the demo records'
    // fixed timestamps identically on every machine.
    cmd.env("TZ", "UTC");
    // The details popup shows the working directory relative to the home directory.
    cmd.env("HOME", home);
    cmd.env("MICROMUX_STATE_DIR", state_dir);
    // The mouse-selection scenario copies, which must not reach the desktop's clipboard.
    cmd.env_remove("DISPLAY");
    cmd.env_remove("WAYLAND_DISPLAY");
    // micromux discovers `micromux.yaml` in its working directory, so no --config is needed.

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|err| Error::operation("failed to spawn micromux", err))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| Error::operation("failed to clone pty reader", err))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|err| Error::operation("failed to take pty writer", err))?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader_thread = std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf.get(..n).unwrap_or_default().to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut parser = vt100::Parser::new(scenario.rows, scenario.cols, 0);

    // A blank screen holds still as well, so wait for the first frame before settling; keys
    // typed earlier would reach a terminal that is not in raw mode yet.
    if !wait_for_first_frame(&rx, &mut parser, &mut writer) {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(Error::EarlyExit {
                status,
                example_dir: example_dir.to_path_buf(),
            });
        }
        let _ = child.kill();
        return Err(Error::NoFirstFrame {
            example_dir: example_dir.to_path_buf(),
        });
    }
    settle(&rx, &mut parser);
    if let Ok(Some(status)) = child.try_wait() {
        return Err(Error::EarlyExit {
            status,
            example_dir: example_dir.to_path_buf(),
        });
    }

    for keys in scenario.keys {
        writer.write_all(keys)?;
        writer.flush().ok();
        settle(&rx, &mut parser);
    }

    let screen = parser.screen().clone();

    // Quit micromux gracefully (`q`) so its scheduler tears down the demo service process groups
    // instead of leaving the long-lived `sleep` children orphaned.
    writer.write_all(b"q").ok();
    writer.flush().ok();
    drop(writer);
    wait_for_exit(child.as_mut());
    drop(rx);
    let _ = reader_thread.join();

    Ok(screen)
}

/// Feed PTY output into `parser` until the TUI drew its first frame, which shows the services
/// pane; returns whether it did within the time limit.
///
/// micromux asks the terminal at startup whether it has the kitty keyboard protocol, and waits
/// up to two seconds for an answer; like a terminal without the protocol, this answers only the
/// device attributes query that follows, so the answer comes at once.
fn wait_for_first_frame(
    rx: &mpsc::Receiver<Vec<u8>>,
    parser: &mut vt100::Parser,
    writer: &mut impl Write,
) -> bool {
    const MAX_WAIT: Duration = Duration::from_secs(30);
    const ATTRIBUTES_QUERY: &[u8] = b"\x1b[c";

    let deadline = Instant::now() + MAX_WAIT;
    let mut written = Vec::new();
    let mut answered = false;
    while Instant::now() < deadline {
        if parser.screen().contents().contains("Services") {
            return true;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => {
                parser.process(&chunk);
                written.extend_from_slice(&chunk);
                if !answered
                    && written
                        .windows(ATTRIBUTES_QUERY.len())
                        .any(|window| window == ATTRIBUTES_QUERY)
                {
                    answered = true;
                    // Without an answer micromux only starts two seconds later.
                    writer.write_all(b"\x1b[?62c").ok();
                    writer.flush().ok();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
    false
}

/// Drive the parser until the rendered screen stops changing.
///
/// ratatui only writes the cell diff on each redraw, so a stable UI produces no further output —
/// but periodic ticks can still emit tiny updates. Rather than wait for byte silence we wait for
/// the formatted screen contents to hold steady for `STABLE_FOR`, bounded by `MAX_WAIT`.
fn settle(rx: &mpsc::Receiver<Vec<u8>>, parser: &mut vt100::Parser) {
    const POLL: Duration = Duration::from_millis(100);
    const STABLE_FOR: Duration = Duration::from_millis(600);
    const MAX_WAIT: Duration = Duration::from_secs(8);

    let deadline = Instant::now() + MAX_WAIT;
    let mut last = parser.screen().contents_formatted();
    let mut stable_since = Instant::now();

    loop {
        match rx.recv_timeout(POLL) {
            Ok(chunk) => {
                parser.process(&chunk);
                while let Ok(chunk) = rx.try_recv() {
                    parser.process(&chunk);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        let now = Instant::now();
        let snapshot = parser.screen().contents_formatted();
        if snapshot == last {
            if now.duration_since(stable_since) >= STABLE_FOR {
                break;
            }
        } else {
            last = snapshot;
            stable_since = now;
        }
        if now >= deadline {
            break;
        }
    }
}

/// Wait briefly for the child to exit on its own, then kill it if it is still alive.
fn wait_for_exit(child: &mut dyn portable_pty::Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }
}
