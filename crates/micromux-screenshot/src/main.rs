//! Capture README screenshots of the real micromux TUI.
//!
//! This crate is never published. It deliberately uses **no micromux internals**: it runs the
//! actual `micromux` binary inside a pseudo-terminal, lets the UI settle, scrapes the rendered
//! screen with a `vt100` terminal emulator, and renders the result to a PNG with
//! [`freeze`](https://github.com/charmbracelet/freeze). Driving the published binary the same way a
//! user would keeps the screenshots honest and keeps all of this tooling out of the published
//! crates.

use color_eyre::eyre::{self, WrapErr as _, eyre};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// A single screenshot to capture from the demo config.
struct Scenario {
    /// Output basename (writes `docs/<name>.png`).
    name: &'static str,
    /// Terminal width in columns.
    cols: u16,
    /// Terminal height in rows.
    rows: u16,
    /// Keystrokes to send (each chunk is settled before the next), e.g. `b"H"` to open the
    /// healthcheck pane. Empty captures the default frame the config settles into.
    keys: &'static [&'static [u8]],
}

const SCENARIOS: &[Scenario] = &[
    // The dashboard: the default frame the config settles into, `api` selected with its logs.
    Scenario {
        name: "overview",
        cols: 180,
        rows: 44,
        keys: &[],
    },
    // Healthcheck diagnostics: navigate down to the unhealthy `payments` service (6 rows below the
    // initial `api` selection), open the healthcheck pane with `H`, then enable wrap with `w` so the
    // long probe command line is readable instead of truncated.
    Scenario {
        name: "healthcheck",
        cols: 180,
        rows: 44,
        keys: &[b"jjjjjj" as &[u8], b"H", b"w"],
    },
    // Service management: select `web` (one row below `api`) and disable it with `d`. Its sidebar
    // row turns gray (DISABLED) and the process is stopped, while its captured logs remain.
    Scenario {
        name: "disable",
        cols: 180,
        rows: 44,
        keys: &[b"j" as &[u8], b"d"],
    },
];

fn main() -> eyre::Result<()> {
    color_eyre::install()?;

    let workspace = workspace_dir()?;
    let micromux = micromux_bin(&workspace)?;
    let example_dir = workspace.join("examples").join("demo");
    let docs = workspace.join("docs");

    ensure_freeze()?;
    std::fs::create_dir_all(&docs).wrap_err("failed to create docs directory")?;

    for scenario in SCENARIOS {
        let ansi = capture(&micromux, &example_dir, scenario)
            .wrap_err_with(|| format!("failed to capture scenario `{}`", scenario.name))?;

        let tmp = std::env::temp_dir().join(format!("micromux-screenshot-{}.ansi", scenario.name));
        std::fs::write(&tmp, &ansi).wrap_err("failed to write captured ANSI")?;

        let png = docs.join(format!("{}.png", scenario.name));
        run_freeze(&tmp, &png)?;
        let _ = std::fs::remove_file(&tmp);
        println!("wrote {}", png.display());
    }

    Ok(())
}

/// Absolute path to the workspace root (this crate lives at `crates/micromux-screenshot`).
fn workspace_dir() -> eyre::Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .wrap_err("failed to resolve workspace root")
}

/// Locate the `micromux` binary: `$MICROMUX_BIN` if set, otherwise the sibling of this tool's own
/// executable (the same `target/<profile>` directory).
fn micromux_bin(workspace: &Path) -> eyre::Result<PathBuf> {
    if let Some(path) = std::env::var_os("MICROMUX_BIN") {
        return Ok(PathBuf::from(path));
    }

    let exe = std::env::current_exe().wrap_err("failed to resolve current executable")?;
    let dir = exe
        .parent()
        .ok_or_else(|| eyre!("current executable has no parent directory"))?;
    let name = if cfg!(windows) { "micromux.exe" } else { "micromux" };
    let candidate = dir.join(name);
    if candidate.exists() {
        return Ok(candidate);
    }

    Err(eyre!(
        "micromux binary not found at {} — build it first with `cargo build -p micromux-cli` \
         (or point $MICROMUX_BIN at it).\nworkspace: {}",
        candidate.display(),
        workspace.display()
    ))
}

/// Fail early with a helpful message if `freeze` is not installed.
fn ensure_freeze() -> eyre::Result<()> {
    let ok = std::process::Command::new("freeze")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if ok {
        Ok(())
    } else {
        Err(eyre!(
            "`freeze` not found — install it from https://github.com/charmbracelet/freeze"
        ))
    }
}

/// Run `micromux` in a PTY against `example_dir`, drive the scenario, and return the settled screen
/// as a plain ANSI string (one row per line).
fn capture(micromux: &Path, example_dir: &Path, scenario: &Scenario) -> eyre::Result<String> {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: scenario.rows,
            cols: scenario.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| eyre!("failed to open pty: {err}"))?;

    let mut cmd = CommandBuilder::new(micromux);
    cmd.cwd(example_dir);
    cmd.env("TERM", "xterm-256color");
    // micromux discovers `micromux.yaml` in its working directory, so no --config is needed.

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|err| eyre!("failed to spawn micromux: {err}"))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| eyre!("failed to clone pty reader: {err}"))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|err| eyre!("failed to take pty writer: {err}"))?;

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

    settle(&rx, &mut parser);
    if let Ok(Some(status)) = child.try_wait() {
        return Err(eyre!(
            "micromux exited before it could be captured (status: {status:?}); \
             check the demo config in {}",
            example_dir.display()
        ));
    }

    for keys in scenario.keys {
        writer.write_all(keys).wrap_err("failed to send keystroke")?;
        writer.flush().ok();
        settle(&rx, &mut parser);
    }

    let ansi = screen_to_ansi(parser.screen(), scenario.rows, scenario.cols);

    // Quit micromux gracefully (`q`) so its scheduler tears down the demo service process groups
    // instead of leaving the long-lived `sleep` children orphaned.
    writer.write_all(b"q").ok();
    writer.flush().ok();
    drop(writer);
    wait_for_exit(child.as_mut());
    drop(rx);
    let _ = reader_thread.join();

    Ok(ansi)
}

/// Drive the parser until the rendered screen stops changing.
///
/// ratatui only writes the cell diff on each redraw, so a stable UI produces no further output —
/// but periodic ticks can still emit tiny updates. Rather than wait for byte silence we wait for
/// the formatted screen contents to hold steady for [`STABLE_FOR`], bounded by [`MAX_WAIT`].
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

/// A 24-bit/indexed color, in our own representation so equality (for run coalescing) does not
/// depend on `vt100`'s derives.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Col {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

impl From<vt100::Color> for Col {
    fn from(color: vt100::Color) -> Self {
        match color {
            vt100::Color::Default => Self::Default,
            vt100::Color::Idx(i) => Self::Idx(i),
            vt100::Color::Rgb(r, g, b) => Self::Rgb(r, g, b),
        }
    }
}

/// The effective style of a single cell.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Sty {
    fg: Col,
    bg: Col,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

const DEFAULT_STY: Sty = Sty {
    fg: Col::Default,
    bg: Col::Default,
    bold: false,
    dim: false,
    italic: false,
    underline: false,
    inverse: false,
};

/// Serialize the emulator screen to a plain ANSI string, walking every cell and emitting explicit
/// spaces for blank cells.
///
/// We do not use `vt100`'s own `rows_formatted`/`contents_formatted`: those coalesce runs of blank
/// cells into cursor-forward (`CUF`) escapes, which `freeze --language text` does not expand back
/// into spaces, so columns collapse. Emitting real spaces — exactly what a terminal shows — renders
/// faithfully. Each styled run is introduced by a full reset so the bare `\x1b[39m`/`\x1b[49m`
/// codes (which `freeze` ignores) never appear.
fn screen_to_ansi(screen: &vt100::Screen, rows: u16, cols: u16) -> String {
    let mut out = String::new();
    for row in 0..rows {
        let mut current = DEFAULT_STY;
        for col in 0..cols {
            let cell = screen.cell(row, col);
            if cell.is_some_and(vt100::Cell::is_wide_continuation) {
                continue;
            }

            let (text, sty) = match cell {
                Some(cell) => {
                    let text = if cell.has_contents() {
                        cell.contents()
                    } else {
                        " "
                    };
                    (
                        text,
                        Sty {
                            fg: cell.fgcolor().into(),
                            bg: cell.bgcolor().into(),
                            bold: cell.bold(),
                            dim: cell.dim(),
                            italic: cell.italic(),
                            underline: cell.underline(),
                            inverse: cell.inverse(),
                        },
                    )
                }
                None => (" ", DEFAULT_STY),
            };

            if sty != current {
                out.push_str("\x1b[0");
                if sty.bold {
                    out.push_str(";1");
                }
                if sty.dim {
                    out.push_str(";2");
                }
                if sty.italic {
                    out.push_str(";3");
                }
                if sty.underline {
                    out.push_str(";4");
                }
                if sty.inverse {
                    out.push_str(";7");
                }
                push_color(&mut out, sty.fg, false);
                push_color(&mut out, sty.bg, true);
                out.push('m');
                current = sty;
            }
            out.push_str(text);
        }
        if current != DEFAULT_STY {
            out.push_str("\x1b[0m");
        }
        out.push('\n');
    }
    out
}

/// Append the SGR parameters for `color` (each with a leading `;`); `Default` appends nothing.
fn push_color(out: &mut String, color: Col, background: bool) {
    let (extended, indexed) = if background {
        (";48;2;", ";48;5;")
    } else {
        (";38;2;", ";38;5;")
    };
    match color {
        Col::Default => {}
        Col::Idx(i) => {
            out.push_str(indexed);
            out.push_str(&i.to_string());
        }
        Col::Rgb(r, g, b) => {
            out.push_str(extended);
            let _ = write!(out, "{r};{g};{b}");
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

/// Render a captured ANSI file to a PNG with `freeze`, using window chrome that matches the docs.
fn run_freeze(input: &Path, output: &Path) -> eyre::Result<()> {
    let status = std::process::Command::new("freeze")
        .arg("--language")
        .arg("text")
        .arg("--output")
        .arg(output)
        .arg("--window")
        .arg("--shadow.blur")
        .arg("20")
        .arg("--border.radius")
        .arg("8")
        .arg("--padding")
        .arg("20")
        .arg(input)
        .status()
        .wrap_err("failed to run freeze")?;

    if status.success() {
        Ok(())
    } else {
        Err(eyre!("freeze failed for {}", input.display()))
    }
}
