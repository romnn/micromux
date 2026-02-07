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
use vt100::Parser;

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
    alt_screen_change: Option<bool>,
    saw_non_sgr_csi: bool,
}

impl AnsiFilter {
    fn new() -> Self {
        Self {
            state: AnsiState::Ground,
            esc_seen: false,
            csi_buf: Vec::new(),
            alt_screen_change: None,
            saw_non_sgr_csi: false,
        }
    }

    fn take_alt_screen_change(&mut self) -> Option<bool> {
        self.alt_screen_change.take()
    }

    fn take_saw_non_sgr_csi(&mut self) -> bool {
        std::mem::take(&mut self.saw_non_sgr_csi)
    }

    /// Feed one byte into the filter. Printable text and SGR color
    /// sequences are appended to `out`. Returns `true` when a
    /// cursor-positioning or screen-clearing CSI sequence just
    /// finished, signalling the caller to flush accumulated text
    /// (this turns ncurses-style screen redraws into discrete lines).
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
                        if matches!(b, b'h' | b'l')
                            && self.csi_buf.len() >= 5
                            && self.csi_buf.starts_with(&[0x1b, b'[', b'?'])
                        {
                            let set = b == b'h';
                            let mut cur: u16 = 0;
                            let mut have = false;
                            let mut matched = false;
                            if let Some(params) = self.csi_buf.get(3..self.csi_buf.len() - 1) {
                                for &ch in params {
                                    match ch {
                                        b'0'..=b'9' => {
                                            have = true;
                                            cur = cur
                                                .saturating_mul(10)
                                                .saturating_add(u16::from(ch - b'0'));
                                        }
                                        b';' => {
                                            if have && matches!(cur, 47 | 1047 | 1049) {
                                                matched = true;
                                            }
                                            cur = 0;
                                            have = false;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            if have && matches!(cur, 47 | 1047 | 1049) {
                                matched = true;
                            }
                            if matched {
                                self.alt_screen_change = Some(set);
                            }
                        }
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

fn spawn_log_reader_thread(
    service_id: ServiceID,
    reader: Box<dyn std::io::Read + Send>,
    events_tx: mpsc::Sender<Event>,
    pty_rows: u16,
    pty_cols: u16,
    pty_size: Arc<AtomicU32>,
) {
    thread::spawn(move || {
        let mut reader = std::io::BufReader::new(reader);
        let mut buf = [0u8; 4096];
        let mut line: Vec<u8> = Vec::new();
        let mut scratch: Vec<u8> = Vec::new();
        let mut filter = AnsiFilter::new();
        let mut term = Parser::new(pty_rows, pty_cols, 0);
        let mut interactive = false;
        let mut last_snapshot_at: Option<Instant> = None;
        let mut dirty = false;
        let mut last_size = 0u32;

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

        const ALT_SCREEN_MAX_UPDATES_PER_SEC: u32 = 4;

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
            term: &Parser,
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

            let screen = term.screen();
            let (rows, cols) = screen.size();
            let plain_rows: Vec<String> = screen.rows(0, cols).take(rows as usize).collect();
            let mut last_non_empty = None;
            for (idx, row) in plain_rows.iter().enumerate() {
                if !row.trim_end().is_empty() {
                    last_non_empty = Some(idx);
                }
            }
            let Some(last_non_empty) = last_non_empty else {
                return;
            };

            let formatted_rows: Vec<Vec<u8>> = screen
                .rows_formatted(0, cols)
                .take(last_non_empty + 1)
                .collect();
            let mut snapshot = String::new();
            for (i, row) in formatted_rows.into_iter().enumerate() {
                if i > 0 {
                    snapshot.push('\n');
                }
                snapshot.push_str(&String::from_utf8_lossy(&row));
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

        let mut rate = RateLimit::new();

        loop {
            let size = pty_size.load(Ordering::Relaxed);
            if size != 0 && size != last_size {
                last_size = size;
                let rows = (size >> 16) as u16;
                let cols = (size & 0xffff) as u16;
                term.screen_mut().set_size(rows, cols);
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
            term.process(chunk);
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

                        if let Some(new_alt) = filter.take_alt_screen_change() {
                            rate.set_alt_screen(new_alt);
                            interactive = true;
                            dirty = true;
                            line.clear();
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
