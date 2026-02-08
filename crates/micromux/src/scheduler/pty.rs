use super::{Event, LogUpdateKind, OutputStream, ServiceID};
use crate::{health_check, service::Service};
use color_eyre::eyre;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use alacritty_terminal::{
    event::{Event as AlacrittyEvent, EventListener, WindowSize},
    grid::Dimensions as _,
    term::{Config as AlacrittyConfig, Term, TermMode},
    vte::ansi,
};

#[cfg(unix)]
use nix::sys::signal::Signal;

#[cfg(unix)]
use nix::unistd::Pid;

#[derive(Clone)]
pub(super) struct PtyHandles {
    pub(super) master: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    pub(super) writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub(super) size: Arc<AtomicU32>,
}

#[derive(Clone, Copy, Debug)]
struct TermSize {
    columns: usize,
    screen_lines: usize,
}

impl alacritty_terminal::grid::Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

fn env_vars_for_service(service: &Service) -> HashMap<String, String> {
    let mut env_vars: HashMap<String, String> = service
        .environment
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    if service.enable_color {
        env_vars.insert("TERM".to_string(), "xterm-256color".to_string());
        env_vars.insert("CLICOLOR".to_string(), "1".to_string());
        env_vars.insert("CLICOLOR_FORCE".to_string(), "1".to_string());
        env_vars.insert("FORCE_COLOR".to_string(), "1".to_string());
    }

    env_vars
}

/// Streaming ANSI escape sequence filter.
///
/// Consumes escape sequences byte-by-byte, preserving only SGR color
/// sequences (`ESC[...m`) and dropping all other control sequences
/// (cursor movement, screen clears, charset switches, OSC, DCS, etc.).
/// Printable bytes and tabs are passed through to the output buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AnsiState {
    Ground,
    Esc,
    Csi,
    Osc,
    Dcs,
    Pm,
    Apc,
    Charset,
}

struct AnsiFilter {
    state: AnsiState,
    esc_seen: bool,
    csi_buf: Vec<u8>,
    saw_non_sgr_csi: bool,
}

impl AnsiFilter {
    fn new() -> Self {
        Self {
            state: AnsiState::Ground,
            esc_seen: false,
            csi_buf: Vec::new(),
            saw_non_sgr_csi: false,
        }
    }

    fn take_saw_non_sgr_csi(&mut self) -> bool {
        std::mem::take(&mut self.saw_non_sgr_csi)
    }

    /// Feed one byte into the filter. Printable text and SGR color
    /// sequences are appended to `out`. Returns `true` when a
    /// cursor-positioning or screen-clearing CSI sequence just
    /// finished, signalling the caller to flush accumulated text
    /// (this turns ncurses-style screen redraws into discrete lines).
    #[allow(clippy::too_many_lines)]
    fn push(&mut self, b: u8, out: &mut Vec<u8>) -> bool {
        match self.state {
            AnsiState::Ground => {
                if b == 0x1b {
                    self.state = AnsiState::Esc;
                } else if b.is_ascii_control() {
                    if b == b'\t' {
                        out.push(b);
                    }
                } else {
                    out.push(b);
                }
                false
            }
            AnsiState::Esc => {
                self.state = AnsiState::Ground;
                match b {
                    b'[' => {
                        self.state = AnsiState::Csi;
                        self.csi_buf.clear();
                        self.csi_buf.push(0x1b);
                        self.csi_buf.push(b'[');
                    }
                    b']' => {
                        self.state = AnsiState::Osc;
                        self.esc_seen = false;
                    }
                    b'P' => {
                        self.state = AnsiState::Dcs;
                        self.esc_seen = false;
                    }
                    b'^' => {
                        self.state = AnsiState::Pm;
                        self.esc_seen = false;
                    }
                    b'_' => {
                        self.state = AnsiState::Apc;
                        self.esc_seen = false;
                    }
                    b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' | b'%' | b'#' => {
                        self.state = AnsiState::Charset;
                    }
                    _ => {}
                }
                false
            }
            AnsiState::Charset => {
                self.state = AnsiState::Ground;
                false
            }
            AnsiState::Csi => {
                self.csi_buf.push(b);
                if self.csi_buf.len() > 1024 {
                    self.csi_buf.clear();
                    self.state = AnsiState::Ground;
                    return false;
                }
                if (0x40..=0x7e).contains(&b) {
                    self.state = AnsiState::Ground;
                    if b == b'm' {
                        out.extend_from_slice(&self.csi_buf);
                        self.csi_buf.clear();
                        false
                    } else {
                        self.saw_non_sgr_csi = true;
                        self.csi_buf.clear();
                        matches!(b, b'J')
                    }
                } else {
                    false
                }
            }
            AnsiState::Osc | AnsiState::Dcs | AnsiState::Pm | AnsiState::Apc => {
                if self.esc_seen {
                    self.esc_seen = false;
                    if b == b'\\' {
                        self.state = AnsiState::Ground;
                        return false;
                    }
                }
                if self.state == AnsiState::Osc && b == 0x07 {
                    self.state = AnsiState::Ground;
                    return false;
                }
                if b == 0x1b {
                    self.esc_seen = true;
                }
                false
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SgrColor {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellAttrs(u8);

impl CellAttrs {
    const BOLD: u8 = 1 << 0;
    const DIM: u8 = 1 << 1;
    const ITALIC: u8 = 1 << 2;
    const UNDERLINE: u8 = 1 << 3;
    const INVERSE: u8 = 1 << 4;

    const fn empty() -> Self {
        Self(0)
    }

    const fn contains(self, flag: u8) -> bool {
        (self.0 & flag) != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CellStyle {
    fg: SgrColor,
    bg: SgrColor,
    attrs: CellAttrs,
}

const DEFAULT_CELL_STYLE: CellStyle = CellStyle {
    fg: SgrColor::Default,
    bg: SgrColor::Default,
    attrs: CellAttrs::empty(),
};

fn named_to_sgr_color(
    color: alacritty_terminal::vte::ansi::NamedColor,
    colors: &alacritty_terminal::term::color::Colors,
    is_fg: bool,
) -> SgrColor {
    let idx = color as usize;
    if (is_fg && idx == 256) || (!is_fg && idx == 257) {
        return SgrColor::Default;
    }

    if let Ok(idx) = u8::try_from(idx) {
        return SgrColor::Idx(idx);
    }

    let Some(rgb) = colors[idx] else {
        return SgrColor::Default;
    };

    SgrColor::Rgb(rgb.r, rgb.g, rgb.b)
}

fn color_to_sgr_color(
    color: alacritty_terminal::vte::ansi::Color,
    colors: &alacritty_terminal::term::color::Colors,
    is_fg: bool,
) -> SgrColor {
    match color {
        alacritty_terminal::vte::ansi::Color::Named(named) => {
            named_to_sgr_color(named, colors, is_fg)
        }
        alacritty_terminal::vte::ansi::Color::Indexed(idx) => SgrColor::Idx(idx),
        alacritty_terminal::vte::ansi::Color::Spec(rgb) => SgrColor::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

fn cell_style(
    cell: &alacritty_terminal::term::cell::Cell,
    colors: &alacritty_terminal::term::color::Colors,
) -> CellStyle {
    let flags = cell.flags;
    let mut attrs = CellAttrs::empty();
    if flags.contains(alacritty_terminal::term::cell::Flags::BOLD) {
        attrs.0 |= CellAttrs::BOLD;
    }
    if flags.contains(alacritty_terminal::term::cell::Flags::DIM) {
        attrs.0 |= CellAttrs::DIM;
    }
    if flags.contains(alacritty_terminal::term::cell::Flags::ITALIC) {
        attrs.0 |= CellAttrs::ITALIC;
    }
    if flags.intersects(alacritty_terminal::term::cell::Flags::ALL_UNDERLINES) {
        attrs.0 |= CellAttrs::UNDERLINE;
    }
    if flags.contains(alacritty_terminal::term::cell::Flags::INVERSE) {
        attrs.0 |= CellAttrs::INVERSE;
    }

    CellStyle {
        fg: color_to_sgr_color(cell.fg, colors, true),
        bg: color_to_sgr_color(cell.bg, colors, false),
        attrs,
    }
}

fn push_sgr(snapshot: &mut String, style: CellStyle) {
    use std::fmt::Write as _;

    snapshot.push_str("\x1b[");
    snapshot.push('0');
    if style.attrs.contains(CellAttrs::BOLD) {
        snapshot.push_str(";1");
    }
    if style.attrs.contains(CellAttrs::DIM) {
        snapshot.push_str(";2");
    }
    if style.attrs.contains(CellAttrs::ITALIC) {
        snapshot.push_str(";3");
    }
    if style.attrs.contains(CellAttrs::UNDERLINE) {
        snapshot.push_str(";4");
    }
    if style.attrs.contains(CellAttrs::INVERSE) {
        snapshot.push_str(";7");
    }

    match style.fg {
        SgrColor::Default => {}
        SgrColor::Idx(idx) => {
            let _ = write!(snapshot, ";38;5;{idx}");
        }
        SgrColor::Rgb(r, g, b) => {
            let _ = write!(snapshot, ";38;2;{r};{g};{b}");
        }
    }

    match style.bg {
        SgrColor::Default => {}
        SgrColor::Idx(idx) => {
            let _ = write!(snapshot, ";48;5;{idx}");
        }
        SgrColor::Rgb(r, g, b) => {
            let _ = write!(snapshot, ";48;2;{r};{g};{b}");
        }
    }

    snapshot.push('m');
}

#[allow(clippy::too_many_lines)]
fn spawn_log_reader_thread(
    service_id: ServiceID,
    reader: Box<dyn std::io::Read + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    events_tx: mpsc::Sender<Event>,
    pty_rows: u16,
    pty_cols: u16,
    pty_size: Arc<AtomicU32>,
) {
    thread::spawn(move || {
        #[derive(Clone)]
        struct PtyEventProxy {
            writer: Arc<Mutex<Box<dyn Write + Send>>>,
            pty_size: Arc<AtomicU32>,
        }

        impl EventListener for PtyEventProxy {
            fn send_event(&self, event: AlacrittyEvent) {
                let text = match event {
                    AlacrittyEvent::PtyWrite(text) => Some(text),
                    AlacrittyEvent::TextAreaSizeRequest(formatter) => {
                        let size = self.pty_size.load(Ordering::Relaxed);
                        if size == 0 {
                            return;
                        }
                        let rows = (size >> 16) as u16;
                        let cols = (size & 0xffff) as u16;
                        Some(formatter(WindowSize {
                            num_lines: rows,
                            num_cols: cols,
                            cell_width: 0,
                            cell_height: 0,
                        }))
                    }
                    _ => None,
                };

                let Some(text) = text else {
                    return;
                };

                let mut guard = self.writer.lock();
                if guard.write_all(text.as_bytes()).is_ok() {
                    let _ = guard.flush();
                }
            }
        }

        struct RateLimit {
            alt_screen: bool,
            window_start: Instant,
            sent_in_window: u32,
            warned_in_window: bool,
            have_snapshot: bool,
        }

        impl RateLimit {
            fn new() -> Self {
                Self {
                    alt_screen: false,
                    window_start: Instant::now(),
                    sent_in_window: 0,
                    warned_in_window: false,
                    have_snapshot: false,
                }
            }

            fn set_alt_screen(&mut self, alt_screen: bool) {
                self.alt_screen = alt_screen;
                self.window_start = Instant::now();
                self.sent_in_window = 0;
                self.warned_in_window = false;
                self.have_snapshot = false;
            }
        }

        fn send_log(
            events_tx: &mpsc::Sender<Event>,
            service_id: &ServiceID,
            update: LogUpdateKind,
            text: String,
        ) {
            let _ = events_tx.try_send(Event::LogLine {
                service_id: service_id.clone(),
                stream: OutputStream::Stdout,
                update,
                line: text,
            });
        }

        fn emit_snapshot(
            term: &Term<PtyEventProxy>,
            rate: &mut RateLimit,
            events_tx: &mpsc::Sender<Event>,
            service_id: &ServiceID,
        ) {
            if rate.alt_screen {
                let now = Instant::now();
                if now.duration_since(rate.window_start) >= Duration::from_secs(1) {
                    rate.window_start = now;
                    rate.sent_in_window = 0;
                    rate.warned_in_window = false;
                }
                if rate.sent_in_window >= ALT_SCREEN_MAX_UPDATES_PER_SEC {
                    if !rate.warned_in_window {
                        rate.warned_in_window = true;
                        send_log(
                            events_tx,
                            service_id,
                            LogUpdateKind::Append,
                            "[micromux] interactive output rate-limited".to_string(),
                        );
                    }
                    return;
                }
            }

            let _rows = term.screen_lines();
            let cols = term.columns();
            let content = term.renderable_content();

            let mut snapshot = String::new();
            let mut cur_style = DEFAULT_CELL_STYLE;
            let mut skip_next_wide = false;

            for indexed in content.display_iter {
                let cell = indexed.cell;
                let point = indexed.point;

                if point.column.0 == 0 {
                    if !snapshot.is_empty() {
                        snapshot.push('\n');
                    }
                    cur_style = DEFAULT_CELL_STYLE;
                    push_sgr(&mut snapshot, cur_style);
                    skip_next_wide = false;
                }

                if skip_next_wide {
                    skip_next_wide = false;
                    if cell
                        .flags
                        .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER)
                    {
                        continue;
                    }
                }

                if cell
                    .flags
                    .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER)
                {
                    continue;
                }

                let style = cell_style(cell, content.colors);
                if style != cur_style {
                    cur_style = style;
                    push_sgr(&mut snapshot, cur_style);
                }

                let mut c = cell.c;
                if cell
                    .flags
                    .contains(alacritty_terminal::term::cell::Flags::HIDDEN)
                {
                    c = ' ';
                }
                snapshot.push(c);

                if let Some(zero_width) = cell.zerowidth() {
                    for &c in zero_width {
                        snapshot.push(c);
                    }
                }

                if cell
                    .flags
                    .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR)
                    && point.column.0 + 1 < cols
                {
                    skip_next_wide = true;
                }
            }

            let update = if rate.have_snapshot {
                LogUpdateKind::ReplaceLast
            } else {
                rate.have_snapshot = true;
                LogUpdateKind::Append
            };

            send_log(events_tx, service_id, update, snapshot);
            if rate.alt_screen {
                rate.sent_in_window = rate.sent_in_window.saturating_add(1);
            }
        }

        fn flush(line: &mut Vec<u8>, events_tx: &mpsc::Sender<Event>, service_id: &ServiceID) {
            if line.is_empty() {
                return;
            }
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            while matches!(line.last(), Some(b' ')) {
                line.pop();
            }
            if line.is_empty() {
                return;
            }

            let s = String::from_utf8_lossy(line).to_string();
            send_log(events_tx, service_id, LogUpdateKind::Append, s);
            line.clear();
        }

        const ALT_SCREEN_MAX_UPDATES_PER_SEC: u32 = 4;

        let mut reader = std::io::BufReader::new(reader);
        let mut buf = [0u8; 4096];
        let mut line: Vec<u8> = Vec::new();
        let mut scratch: Vec<u8> = Vec::new();
        let mut filter = AnsiFilter::new();
        let proxy = PtyEventProxy {
            writer,
            pty_size: pty_size.clone(),
        };
        let size = TermSize {
            columns: usize::from(pty_cols),
            screen_lines: usize::from(pty_rows),
        };
        let config = AlacrittyConfig {
            scrolling_history: 0,
            ..AlacrittyConfig::default()
        };
        let mut term: Term<PtyEventProxy> = Term::new(config, &size, proxy);
        let mut processor: ansi::Processor<ansi::StdSyncHandler> = ansi::Processor::default();
        let mut interactive = false;
        let mut last_snapshot_at: Option<Instant> = None;
        let mut dirty = false;
        let mut last_size = 0u32;
        let mut last_alt_screen = false;

        let mut rate = RateLimit::new();

        loop {
            let size = pty_size.load(Ordering::Relaxed);
            if size != 0 && size != last_size {
                last_size = size;
                let rows = (size >> 16) as u16;
                let cols = (size & 0xffff) as u16;
                term.resize(TermSize {
                    columns: usize::from(cols),
                    screen_lines: usize::from(rows),
                });
                dirty = true;
            }

            let n = match reader.read(&mut buf) {
                Ok(n) => n,
                Err(err) => return Err::<_, std::io::Error>(err),
            };
            if n == 0 {
                if interactive {
                    emit_snapshot(&term, &mut rate, &events_tx, &service_id);
                } else {
                    flush(&mut line, &events_tx, &service_id);
                }
                break;
            }

            let Some(chunk) = buf.get(..n) else {
                continue;
            };

            processor.advance(&mut term, chunk);

            let alt_screen = term.mode().contains(TermMode::ALT_SCREEN);
            if alt_screen != last_alt_screen {
                last_alt_screen = alt_screen;
                rate.set_alt_screen(alt_screen);
                interactive = true;
                dirty = true;
                line.clear();
            }
            if interactive {
                dirty = true;
            }

            for &b in chunk {
                match b {
                    // Both \n and \r trigger a flush. This handles:
                    // - Normal programs: \r\n pairs (flushes on \r,
                    //   then \n flushes empty = noop).
                    // - ncurses apps (watch): ESC[B + \r for line
                    //   breaks (text flushed on \r).
                    // - Cursor-positioned text is also flushed by the
                    //   CSI H/f/J boundary detection in AnsiFilter.
                    b'\n' | b'\r' => {
                        if !interactive {
                            flush(&mut line, &events_tx, &service_id);
                        }
                    }
                    _ => {
                        if interactive {
                            scratch.clear();
                            let _ = filter.push(b, &mut scratch);
                        } else {
                            let boundary = filter.push(b, &mut line);
                            if boundary || filter.take_saw_non_sgr_csi() {
                                interactive = true;
                                dirty = true;
                                rate.have_snapshot = false;
                                line.clear();
                            }
                        }

                        if !interactive && line.len() >= 16 * 1024 {
                            flush(&mut line, &events_tx, &service_id);
                        }
                    }
                }
            }

            if interactive {
                let interval = Duration::from_millis(250);
                let now = Instant::now();
                let due = last_snapshot_at.is_none_or(|t| now.duration_since(t) >= interval);
                if dirty && due {
                    emit_snapshot(&term, &mut rate, &events_tx, &service_id);
                    last_snapshot_at = Some(now);
                    dirty = false;
                }
            }
        }

        Ok::<_, std::io::Error>(())
    });
}

struct TerminationTaskArgs {
    service_id: ServiceID,
    events_tx: mpsc::Sender<Event>,
    shutdown: CancellationToken,
    terminate: CancellationToken,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    pid: Option<u32>,
    process_group_leader_id: Option<i32>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

fn spawn_termination_task(args: TerminationTaskArgs) {
    tokio::spawn(async move {
        let TerminationTaskArgs {
            service_id,
            events_tx,
            shutdown,
            terminate,
            mut killer,
            pid,
            process_group_leader_id,
            mut child,
        } = args;

        let mut termination_started = false;
        let mut hard_killed = false;
        #[cfg(unix)]
        let mut kill_deadline: Option<tokio::time::Instant> = None;
        #[cfg(not(unix))]
        let kill_deadline: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                () = shutdown.cancelled(), if !termination_started => {
                    tracing::info!(pid, service_id, "killing process");
                    let _ = events_tx.send(Event::Killed(service_id.clone())).await;
                    #[cfg(unix)]
                    {
                        if let Some(pgid) = process_group_leader_id {
                            let _ = nix::sys::signal::killpg(Pid::from_raw(pgid), Signal::SIGTERM);
                        } else if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
                            let _ = nix::sys::signal::kill(Pid::from_raw(pid), Signal::SIGTERM);
                        }
                        kill_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(750));
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = process_group_leader_id;
                        let _ = killer.kill();
                        hard_killed = true;
                    }
                    termination_started = true;
                }
                () = terminate.cancelled(), if !termination_started => {
                    tracing::info!(pid, service_id, "killing process");
                    let _ = events_tx.send(Event::Killed(service_id.clone())).await;
                    #[cfg(unix)]
                    {
                        if let Some(pgid) = process_group_leader_id {
                            let _ = nix::sys::signal::killpg(Pid::from_raw(pgid), Signal::SIGTERM);
                        } else if let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) {
                            let _ = nix::sys::signal::kill(Pid::from_raw(pid), Signal::SIGTERM);
                        }
                        kill_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(750));
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = process_group_leader_id;
                        let _ = killer.kill();
                        hard_killed = true;
                    }
                    termination_started = true;
                }
                () = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
            }

            if termination_started
                && !hard_killed
                && let Some(deadline) = kill_deadline
                && tokio::time::Instant::now() >= deadline
            {
                let _ = killer.kill();
                hard_killed = true;
            }

            match child.try_wait() {
                Ok(Some(status)) => {
                    let code = i32::try_from(status.exit_code()).unwrap_or(i32::MAX);
                    let _ = events_tx
                        .send(Event::Exited(service_id.clone(), code))
                        .await;
                    break;
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::error!(?err, "failed to poll process status");
                    let _ = events_tx.send(Event::Exited(service_id.clone(), -1)).await;
                    break;
                }
            }
        }
    });
}

#[allow(clippy::too_many_lines)]
pub(super) async fn start_service_with_pty_size(
    service: &Service,
    events_tx: mpsc::Sender<Event>,
    shutdown: CancellationToken,
    terminate: CancellationToken,
    pty_size: portable_pty::PtySize,
) -> eyre::Result<PtyHandles> {
    use portable_pty::{CommandBuilder, PtySize};

    let service_id = service.id.clone();
    let (prog, args) = &service.command;

    let env_vars = env_vars_for_service(service);
    let env_vars = {
        let mut env_vars = env_vars;
        env_vars
            .entry("TERM".to_string())
            .or_insert_with(|| "xterm-256color".to_string());
        env_vars
    };

    tracing::info!(service_id, prog, ?args, ?env_vars, "start service");

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: pty_size.rows,
            cols: pty_size.cols,
            pixel_width: pty_size.pixel_width,
            pixel_height: pty_size.pixel_height,
        })
        .map_err(|err| eyre::eyre!("failed to open pty: {err}"))?;

    let mut cmd = CommandBuilder::new(prog);
    cmd.args(args);
    if let Some(dir) = &service.working_dir {
        cmd.cwd(dir);
    }
    for (k, v) in &env_vars {
        cmd.env(k, v);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|err| eyre::eyre!("failed to spawn in pty: {err}"))?;

    let pid = child.process_id();
    let killer = child.clone_killer();

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| eyre::eyre!("failed to clone pty reader: {err}"))?;

    let writer = pair
        .master
        .take_writer()
        .map_err(|err| eyre::eyre!("failed to take pty writer: {err}"))?;

    #[cfg(unix)]
    let process_group_leader = pair.master.process_group_leader();
    #[cfg(not(unix))]
    let process_group_leader = None;
    let master = Arc::new(Mutex::new(pair.master));
    let writer = Arc::new(Mutex::new(writer));
    let size = Arc::new(AtomicU32::new(
        (u32::from(pty_size.rows) << 16) | u32::from(pty_size.cols),
    ));

    let _ = events_tx
        .send(Event::Started {
            service_id: service_id.clone(),
        })
        .await;

    spawn_log_reader_thread(
        service_id.clone(),
        reader,
        writer.clone(),
        events_tx.clone(),
        pty_size.rows,
        pty_size.cols,
        size.clone(),
    );

    spawn_termination_task(TerminationTaskArgs {
        service_id: service_id.clone(),
        events_tx: events_tx.clone(),
        shutdown: shutdown.clone(),
        terminate: terminate.clone(),
        killer,
        pid,
        process_group_leader_id: process_group_leader,
        child,
    });

    if let Some(health_check) = service.health_check.clone() {
        tokio::spawn({
            let service_id = service_id.clone();
            let working_dir = service.working_dir.clone();
            let environment: std::collections::HashMap<String, String> = service
                .environment
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let events_tx = events_tx.clone();
            let shutdown = shutdown.clone();
            let terminate = terminate.clone();
            async move {
                health_check::run_loop(
                    health_check,
                    &service_id,
                    working_dir,
                    environment,
                    events_tx,
                    shutdown,
                    terminate,
                )
                .await;
            }
        });
    }

    Ok(PtyHandles {
        master,
        writer,
        size,
    })
}
