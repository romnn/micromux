use super::{LogUpdateKind, OutputStream, ProcessEvent, RunId, ServiceID, input::PreparedPtyInput};
use crate::{health_check, model::RunSink, service::Service};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("service command is empty")]
    EmptyCommand,
    #[error("{operation}: {message}")]
    Operation {
        operation: &'static str,
        message: String,
    },
}

impl Error {
    fn operation(operation: &'static str, error: impl std::fmt::Display) -> Self {
        Self::Operation {
            operation,
            message: error.to_string(),
        }
    }
}

use alacritty_terminal::{
    event::{Event as AlacrittyEvent, EventListener, WindowSize},
    grid::Dimensions as _,
    term::{Config as AlacrittyConfig, Term, TermMode},
    vte::ansi,
};

#[cfg(unix)]
use nix::{errno::Errno, sys::signal::Signal, unistd::Pid};

#[cfg(unix)]
use filedescriptor::{
    AsRawFileDescriptor, FileDescriptor, POLLERR, POLLHUP, POLLIN, POLLOUT, Pipe, pollfd,
};

#[cfg(unix)]
use portable_pty::unix::RawFd;

#[cfg(unix)]
use std::os::fd::AsRawFd;

#[cfg(unix)]
const POLL_EVENTS: i16 = POLLIN | POLLHUP | POLLERR;

const ALT_SCREEN_MAX_UPDATES_PER_SEC: u32 = 4;
const PTY_LOG_LINE_MAX_BYTES: usize = 16 * 1024;

fn bounded_line_split(line: &[u8]) -> usize {
    let limit = PTY_LOG_LINE_MAX_BYTES.min(line.len());
    let mut split = limit;
    while split > 0
        && line
            .get(split)
            .is_some_and(|byte| byte & 0b1100_0000 == 0b1000_0000)
    {
        split = split.saturating_sub(1);
    }
    if split == 0 { limit } else { split }
}

/// Maximum time PTY input may make no write progress.
///
/// Each successful write restarts the interval, so a slow reader may consume a large paste without
/// letting a fully stalled reader occupy the writer worker indefinitely.
#[cfg(unix)]
const PTY_INPUT_WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// Bounds queued write batches when a service stops consuming input.
const PTY_WRITE_QUEUE_CAPACITY: usize = 64;

/// How long output may drain after escalation before the PTY is closed.
///
/// The delay preserves normal tail output while bounding how long an escaped session can retain the
/// slave side and prevent the original child from being reaped.
pub(super) const PTY_HANGUP_AFTER_ESCALATION: Duration = Duration::from_secs(2);
/// Maximum time allowed for an active healthcheck task to finish after its service exits.
pub(super) const HEALTH_TASK_STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// How long termination waits inline before continuing lifecycle notification independently.
const TERMINATION_EVENT_NOTIFY_TIMEOUT: Duration = Duration::from_millis(100);

#[cfg(all(unix, not(target_vendor = "apple")))]
struct PtyPoller;

#[cfg(all(unix, target_vendor = "apple"))]
struct PtyPoller {
    queue: nix::sys::event::Kqueue,
}

#[cfg(all(unix, not(target_vendor = "apple")))]
impl PtyPoller {
    #[expect(
        clippy::unnecessary_wraps,
        reason = "the Apple poller allocates a kqueue; one constructor keeps its callers portable"
    )]
    fn new() -> io::Result<Self> {
        Ok(Self)
    }

    #[expect(
        clippy::unused_self,
        reason = "the Apple backend reuses instance state; one instance API keeps callers target-independent"
    )]
    fn poll(&self, descriptors: &mut [pollfd], timeout: Option<Duration>) -> io::Result<usize> {
        filedescriptor::poll(descriptors, timeout).map_err(|error| match error {
            filedescriptor::Error::Poll(error) | filedescriptor::Error::Io(error) => error,
            error => io::Error::other(error),
        })
    }
}

#[cfg(all(unix, target_vendor = "apple"))]
impl PtyPoller {
    fn new() -> io::Result<Self> {
        // filedescriptor emulates poll with select on macOS, inheriting FD_SETSIZE. A persistent
        // kqueue keeps readiness working above that ceiling without allocating a descriptor on
        // every wait.
        Ok(Self {
            queue: nix::sys::event::Kqueue::new().map_err(io::Error::from)?,
        })
    }

    fn poll(&self, descriptors: &mut [pollfd], timeout: Option<Duration>) -> io::Result<usize> {
        use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent};

        let mut changes = Vec::with_capacity(descriptors.len() * 2);
        for (index, descriptor) in descriptors.iter().enumerate() {
            let identifier = usize::try_from(descriptor.fd).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid poll descriptor")
            })?;
            let user_data = isize::try_from(index)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many descriptors"))?;
            if descriptor.events & POLLIN != 0 {
                changes.push(KEvent::new(
                    identifier,
                    EventFilter::EVFILT_READ,
                    EvFlags::EV_ADD | EvFlags::EV_ENABLE,
                    FilterFlag::empty(),
                    0,
                    user_data,
                ));
            }
            if descriptor.events & POLLOUT != 0 {
                changes.push(KEvent::new(
                    identifier,
                    EventFilter::EVFILT_WRITE,
                    EvFlags::EV_ADD | EvFlags::EV_ENABLE,
                    FilterFlag::empty(),
                    0,
                    user_data,
                ));
            }
        }
        let mut events = vec![
            KEvent::new(
                0,
                EventFilter::EVFILT_READ,
                EvFlags::empty(),
                FilterFlag::empty(),
                0,
                0,
            );
            changes.len().max(1)
        ];
        let timeout = timeout.map(|duration| nix::libc::timespec {
            tv_sec: duration
                .as_secs()
                .try_into()
                .unwrap_or(nix::libc::time_t::MAX),
            tv_nsec: duration.subsec_nanos().into(),
        });
        let count = self
            .queue
            .kevent(&changes, &mut events, timeout)
            .map_err(io::Error::from)?;
        for event in events.into_iter().take(count) {
            let Ok(index) = usize::try_from(event.udata()) else {
                continue;
            };
            let Some(descriptor) = descriptors.get_mut(index) else {
                continue;
            };
            match event.filter() {
                Ok(EventFilter::EVFILT_READ) => descriptor.revents |= POLLIN,
                Ok(EventFilter::EVFILT_WRITE) => descriptor.revents |= POLLOUT,
                _ => descriptor.revents |= POLLERR,
            }
            if event.flags().intersects(EvFlags::EV_EOF) {
                descriptor.revents |= POLLHUP;
            }
            if event.flags().intersects(EvFlags::EV_ERROR) {
                descriptor.revents |= POLLERR;
            }
        }
        Ok(count)
    }

    #[cfg(test)]
    fn raw_fd(&self) -> RawFd {
        use std::os::fd::{AsFd as _, AsRawFd as _};

        self.queue.as_fd().as_raw_fd()
    }
}

#[cfg(unix)]
fn poll_error_is_retryable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
    )
}

type SharedPtyMaster = Arc<Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>>;
type SharedPtyWriteQueue = Arc<Mutex<Option<std_mpsc::SyncSender<PtyWrite>>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(super) enum PtyInputDrop {
    #[error("the input queue is full")]
    QueueFull,
    #[error("the PTY writer has stopped")]
    WriterStopped,
}

struct PtyWriter {
    service_id: ServiceID,
    run_id: RunId,
    events_tx: mpsc::Sender<ProcessEvent>,
    writer: Box<dyn Write + Send>,
    cancellation: PtyCancellation,
    #[cfg(unix)]
    poll_write: FileDescriptor,
    #[cfg(unix)]
    cancel_read: FileDescriptor,
    #[cfg(unix)]
    poller: PtyPoller,
}

#[derive(Debug)]
enum PtyWrite {
    Input(PreparedPtyInput),
    TerminalResponse(Vec<u8>),
}

pub(super) struct PtyHandles {
    master: SharedPtyMaster,
    write_queue: SharedPtyWriteQueue,
    size: Arc<AtomicU32>,
}

/// A teardown capability for disconnecting every PTY endpoint associated with one run.
struct PtyShutdown {
    master: SharedPtyMaster,
    write_queue: SharedPtyWriteQueue,
    reader_cancellation: PtyCancellation,
    writer_cancellation: PtyCancellation,
}

pub(super) struct StartedPty {
    /// Pid of the spawned child, when the pty backend reports one. A missing pid must never fail
    /// the start — it only degrades the runtime-identity snapshot.
    pub(super) pid: Option<u32>,
    pub(super) handles: PtyHandles,
    pub(super) log_reader: LogReaderHandle,
}

pub(super) struct StartServiceParams<'a> {
    pub(super) service: &'a Service,
    pub(super) run_id: RunId,
    pub(super) sink: RunSink,
    pub(super) events_tx: &'a mpsc::Sender<ProcessEvent>,
    pub(super) shutdown: &'a CancellationToken,
    pub(super) terminate: &'a CancellationToken,
    pub(super) pty_size: portable_pty::PtySize,
}

impl PtyHandles {
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unnecessary_wraps,
            reason = "Unix setup is fallible; one constructor keeps the API consistent"
        )
    )]
    fn new(
        service_id: ServiceID,
        run_id: RunId,
        events_tx: mpsc::Sender<ProcessEvent>,
        master: Box<dyn portable_pty::MasterPty + Send>,
        writer: Box<dyn Write + Send>,
        size: Arc<AtomicU32>,
        log_reader: &LogReaderHandle,
    ) -> Result<(Self, PtyShutdown), Error> {
        #[cfg(unix)]
        let poll_write = configure_nonblocking_pty(master.as_ref())?;
        #[cfg(unix)]
        let poller =
            PtyPoller::new().map_err(|err| Error::operation("failed to create pty poller", err))?;
        #[cfg(unix)]
        let (cancel_read, writer_cancellation) = PtyCancellation::pipe()?;
        #[cfg(not(unix))]
        let writer_cancellation = PtyCancellation::new();

        let master = Arc::new(Mutex::new(Some(master)));
        let (writer_tx, writer_rx) = std_mpsc::sync_channel(PTY_WRITE_QUEUE_CAPACITY);
        let write_queue = Arc::new(Mutex::new(Some(writer_tx)));
        spawn_pty_writer_thread(
            PtyWriter {
                service_id,
                run_id,
                events_tx,
                writer,
                cancellation: writer_cancellation.clone(),
                #[cfg(unix)]
                poll_write,
                #[cfg(unix)]
                cancel_read,
                #[cfg(unix)]
                poller,
            },
            writer_rx,
        );
        let shutdown = PtyShutdown {
            master: master.clone(),
            write_queue: write_queue.clone(),
            reader_cancellation: log_reader.cancellation.clone(),
            writer_cancellation,
        };
        Ok((
            Self {
                master,
                write_queue,
                size,
            },
            shutdown,
        ))
    }

    pub(super) fn write_input(&self, input: PreparedPtyInput) -> Result<(), PtyInputDrop> {
        let write_queue = self.write_queue.lock();
        let Some(write_queue) = write_queue.as_ref() else {
            return Err(PtyInputDrop::WriterStopped);
        };
        match write_queue.try_send(PtyWrite::Input(input)) {
            Ok(()) => Ok(()),
            Err(std_mpsc::TrySendError::Disconnected(_)) => Err(PtyInputDrop::WriterStopped),
            Err(std_mpsc::TrySendError::Full(_)) => Err(PtyInputDrop::QueueFull),
        }
    }

    pub(super) fn resize(&self, service_id: &ServiceID, size: portable_pty::PtySize) {
        let packed = (u32::from(size.rows) << 16) | u32::from(size.cols);
        self.size.store(packed, Ordering::Relaxed);

        let master = self.master.lock();
        let Some(master) = master.as_ref() else {
            return;
        };
        if let Err(err) = master.resize(size) {
            tracing::warn!(?err, service_id, "failed to resize pty");
        }
    }
}

impl PtyWriter {
    fn run(mut self, receiver: &std_mpsc::Receiver<PtyWrite>) {
        while !self.cancellation.is_cancelled() {
            let Ok(message) = receiver.recv() else {
                break;
            };
            if self.cancellation.is_cancelled() {
                break;
            }
            match message {
                PtyWrite::Input(input) => {
                    let (data, kind, permit) = input.into_parts();
                    let result = self.write_input(&data);
                    if let Err(err) = result
                        && !self.cancellation.is_cancelled()
                    {
                        tracing::warn!(
                            ?err,
                            service_id = self.service_id,
                            input_kind = kind.as_str(),
                            input_len = data.len(),
                            "failed to write complete pty input; dropping its suffix"
                        );
                        // Never wait for scheduler capacity while this thread owns the PTY writer.
                        // Teardown depends on the endpoint being dropped even if the event channel
                        // is saturated; the warning above remains as the diagnostic fallback.
                        let _ = self.events_tx.try_send(ProcessEvent::InputDropped {
                            service_id: self.service_id.clone(),
                            run_id: self.run_id,
                            input_kind: kind.as_str(),
                            reason: format!("PTY write stopped before the complete payload: {err}"),
                        });
                    }
                    // Keep the reservation until the payload allocation is gone, so neither
                    // retained-input budget undercounts live bytes.
                    drop(data);
                    drop(permit);
                }
                PtyWrite::TerminalResponse(data) => {
                    let _ = self.write_terminal_response(&data);
                }
            }
        }
    }

    #[cfg(unix)]
    fn write_input(&mut self, data: &[u8]) -> io::Result<()> {
        self.write_all(data, Some(PTY_INPUT_WRITE_TIMEOUT))
    }

    #[cfg(not(unix))]
    fn write_input(&mut self, data: &[u8]) -> io::Result<()> {
        // Portable PTY exposes no readiness handle for interrupting a blocked write on these
        // platforms.
        //
        // The dedicated writer thread still keeps the scheduler responsive, but it may retain its
        // endpoint until the platform write returns.
        self.writer.write_all(data)?;
        self.writer.flush()
    }

    #[cfg(unix)]
    fn write_terminal_response(&mut self, data: &[u8]) -> io::Result<()> {
        // Keep a response ahead of later input and retry backpressure without a deadline.
        //
        // This avoids deliberately abandoning a partially written escape sequence, while PTY
        // cancellation still interrupts the wait during teardown.
        self.write_all(data, None)
    }

    #[cfg(not(unix))]
    fn write_terminal_response(&mut self, data: &[u8]) -> io::Result<()> {
        self.writer.write_all(data)?;
        self.writer.flush()
    }

    #[cfg(unix)]
    fn write_all(
        &mut self,
        mut remaining: &[u8],
        stall_timeout: Option<Duration>,
    ) -> io::Result<()> {
        let mut deadline = stall_timeout.and_then(|timeout| Instant::now().checked_add(timeout));
        while !remaining.is_empty() {
            if self.cancellation.is_cancelled() {
                return Err(Self::cancelled());
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(Self::input_timeout());
            }

            match self.writer.write(remaining) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write the complete pty data",
                    ));
                }
                Ok(written) => {
                    remaining = remaining.get(written..).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "pty writer reported more bytes than it received",
                        )
                    })?;
                    deadline =
                        stall_timeout.and_then(|timeout| Instant::now().checked_add(timeout));
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    self.wait_until_writable(deadline)?;
                }
                Err(err) => return Err(err),
            }
        }
        self.writer.flush()
    }

    #[cfg(unix)]
    fn cancelled() -> io::Error {
        io::Error::new(io::ErrorKind::Interrupted, "pty writer was cancelled")
    }

    #[cfg(unix)]
    fn input_timeout() -> io::Error {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "timed out waiting for pty input backpressure to clear",
        )
    }

    #[cfg(unix)]
    fn wait_until_writable(&self, deadline: Option<Instant>) -> io::Result<()> {
        loop {
            let timeout = match deadline {
                Some(deadline) => Some(
                    deadline
                        .checked_duration_since(Instant::now())
                        .ok_or_else(Self::input_timeout)?,
                ),
                None => None,
            };
            let mut descriptors = [
                pollfd {
                    fd: self.poll_write.as_raw_fd(),
                    events: POLLOUT | POLLHUP | POLLERR,
                    revents: 0,
                },
                pollfd {
                    fd: self.cancel_read.as_raw_fd(),
                    events: POLL_EVENTS,
                    revents: 0,
                },
            ];

            match self.poller.poll(&mut descriptors, timeout) {
                Ok(0) => return Err(Self::input_timeout()),
                Ok(_) => {
                    let writer_events = descriptors
                        .first()
                        .map_or(0, |descriptor| descriptor.revents);
                    let cancel_events = descriptors
                        .get(1)
                        .map_or(0, |descriptor| descriptor.revents);
                    if cancel_events & POLL_EVENTS != 0 {
                        return Err(Self::cancelled());
                    }
                    if writer_events & POLLOUT != 0 {
                        return Ok(());
                    }
                    if writer_events & (POLLHUP | POLLERR) != 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "pty closed while waiting to write",
                        ));
                    }
                }
                Err(err) if poll_error_is_retryable(&err) => {}
                Err(err) => return Err(err),
            }
        }
    }
}

fn spawn_pty_writer_thread(writer: PtyWriter, receiver: std_mpsc::Receiver<PtyWrite>) {
    thread::spawn(move || writer.run(&receiver));
}

impl PtyShutdown {
    fn close(self) {
        // The Unix reader and writer workers own duplicated master descriptors, so the slave
        // remains connected until both workers release them.
        //
        // Wake the workers before disconnecting the queue and interactive master.
        //
        // Other platforms cannot interrupt an in-flight portable-pty read or write.
        //
        // Cancellation prevents later queued writes, while endpoint release still depends on the
        // active call returning.
        self.reader_cancellation.cancel();
        self.writer_cancellation.cancel();
        self.write_queue.lock().take();
        self.master.lock().take();
    }
}

impl Drop for PtyShutdown {
    fn drop(&mut self) {
        // Preserve the output reader for post-exit drain, but request writer cancellation as soon
        // as the termination task relinquishes the run.
        //
        // On Unix, this also wakes a terminal response waiting on backpressure without a deadline,
        // preventing it from retaining its master descriptor.
        self.writer_cancellation.cancel();
    }
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
        .spec
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

/// Maximum control-sequence payload before capture resumes in visible-text mode.
pub(super) const ANSI_SEQUENCE_PAYLOAD_MAX_BYTES: usize = 1024;

struct AnsiFilter {
    state: AnsiState,
    esc_seen: bool,
    csi_buf: Vec<u8>,
    string_len: usize,
}

impl AnsiFilter {
    fn new() -> Self {
        Self {
            state: AnsiState::Ground,
            esc_seen: false,
            csi_buf: Vec::new(),
            string_len: 0,
        }
    }

    const fn captures_line_break(&self) -> bool {
        matches!(self.state, AnsiState::Ground)
    }

    fn finish_string(&mut self) {
        self.state = AnsiState::Ground;
        self.esc_seen = false;
        self.string_len = 0;
    }

    fn advance_string_len(&mut self) -> bool {
        self.string_len = self.string_len.saturating_add(1);
        if self.string_len > ANSI_SEQUENCE_PAYLOAD_MAX_BYTES {
            self.finish_string();
            return false;
        }
        true
    }

    fn push_string(&mut self, b: u8) {
        if self.esc_seen {
            self.esc_seen = false;
            if b == b'\\' {
                self.finish_string();
                return;
            }
            // The preceding ESC was payload rather than the first half of the ST terminator.
            if !self.advance_string_len() {
                return;
            }
        }
        if b == 0x9c {
            self.finish_string();
            return;
        }
        if self.state == AnsiState::Osc && b == 0x07 {
            self.finish_string();
            return;
        }
        if b == 0x1b {
            self.esc_seen = true;
        } else {
            let _ = self.advance_string_len();
        }
    }

    /// Feeds one byte into the filter.
    ///
    /// Printable text and SGR color sequences are appended to `out`. Returns `true` when a
    /// screen-clearing CSI sequence finishes, signalling the caller to flush accumulated text.
    ///
    /// Other cursor and progress controls are filtered without changing the log record mode;
    /// wrapping belongs to the TUI renderer, not to captured service logs.
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
                        self.string_len = 0;
                    }
                    b'P' => {
                        self.state = AnsiState::Dcs;
                        self.esc_seen = false;
                        self.string_len = 0;
                    }
                    b'^' => {
                        self.state = AnsiState::Pm;
                        self.esc_seen = false;
                        self.string_len = 0;
                    }
                    b'_' => {
                        self.state = AnsiState::Apc;
                        self.esc_seen = false;
                        self.string_len = 0;
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
                if self.csi_buf.len() > ANSI_SEQUENCE_PAYLOAD_MAX_BYTES + 2 {
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
                        self.csi_buf.clear();
                        matches!(b, b'J')
                    }
                } else {
                    false
                }
            }
            AnsiState::Osc | AnsiState::Dcs | AnsiState::Pm | AnsiState::Apc => {
                self.push_string(b);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapshotDecision {
    Emit,
    Warn,
    Drop,
}

#[derive(Debug)]
struct RateLimit {
    alt_screen: bool,
    window_start: Instant,
    sent_in_window: u32,
    warned_in_window: bool,
    snapshot_id: u64,
}

impl RateLimit {
    fn new() -> Self {
        Self {
            alt_screen: false,
            window_start: Instant::now(),
            sent_in_window: 0,
            warned_in_window: false,
            snapshot_id: 0,
        }
    }

    fn set_alt_screen(&mut self, alt_screen: bool) {
        self.alt_screen = alt_screen;
        self.window_start = Instant::now();
        self.sent_in_window = 0;
        self.warned_in_window = false;
        self.snapshot_id = self.snapshot_id.wrapping_add(1).max(1);
    }

    const fn snapshot_id(&self) -> u64 {
        self.snapshot_id
    }

    fn snapshot_decision(&mut self, force: bool, now: Instant) -> SnapshotDecision {
        // `force` bypasses the rate limiter so the program's final frame at EOF is never
        // dropped just because the current window's update budget was already spent.
        if !self.alt_screen || force {
            return SnapshotDecision::Emit;
        }

        if now.duration_since(self.window_start) >= Duration::from_secs(1) {
            self.window_start = now;
            self.sent_in_window = 0;
            self.warned_in_window = false;
        }

        if self.sent_in_window < ALT_SCREEN_MAX_UPDATES_PER_SEC {
            return SnapshotDecision::Emit;
        }

        if self.warned_in_window {
            SnapshotDecision::Drop
        } else {
            self.warned_in_window = true;
            SnapshotDecision::Warn
        }
    }

    fn record_snapshot_sent(&mut self) {
        if self.alt_screen {
            self.sent_in_window = self.sent_in_window.saturating_add(1);
        }
    }
}

enum PtyOutputReader {
    #[cfg(unix)]
    Polling(PollingPtyReader),
    #[cfg(not(unix))]
    Blocking(std::io::BufReader<Box<dyn Read + Send>>),
}

impl PtyOutputReader {
    fn new(
        master: &(dyn portable_pty::MasterPty + Send),
    ) -> Result<(Self, LogReaderHandle), Error> {
        #[cfg(unix)]
        {
            let (cancel_read, cancellation) = PtyCancellation::pipe()?;
            let reader =
                PollingPtyReader::new(master, cancel_read, cancellation.cancelled.clone())?;
            Ok((Self::Polling(reader), LogReaderHandle { cancellation }))
        }

        #[cfg(not(unix))]
        {
            let reader = master
                .try_clone_reader()
                .map_err(|err| Error::operation("failed to clone pty reader", err))?;
            Ok((
                Self::Blocking(std::io::BufReader::new(reader)),
                LogReaderHandle::new(),
            ))
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<Option<NonZeroUsize>> {
        match self {
            #[cfg(unix)]
            Self::Polling(reader) => reader.read(buf),
            #[cfg(not(unix))]
            Self::Blocking(reader) => Ok(NonZeroUsize::new(reader.read(buf)?)),
        }
    }
}

pub(super) struct LogReaderHandle {
    cancellation: PtyCancellation,
}

/// A cloneable, idempotent cancellation signal for PTY workers.
///
/// Unix workers poll a pipe owned by the signal, so dropping its write end interrupts their
/// readiness waits.
///
/// Other platforms stop queued writes between portable-pty calls, while blocking readers still
/// rely on EOF.
#[derive(Clone)]
struct PtyCancellation {
    cancelled: Arc<AtomicBool>,
    #[cfg(unix)]
    cancel_write: Arc<Mutex<Option<FileDescriptor>>>,
}

impl PtyCancellation {
    #[cfg(unix)]
    fn pipe() -> Result<(FileDescriptor, Self), Error> {
        let pipe = Pipe::new()
            .map_err(|err| Error::operation("failed to create pty cancellation pipe", err))?;
        Ok((
            pipe.read,
            Self {
                cancelled: Arc::new(AtomicBool::new(false)),
                cancel_write: Arc::new(Mutex::new(Some(pipe.write))),
            },
        ))
    }

    #[cfg(not(unix))]
    fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        #[cfg(unix)]
        {
            self.cancel_write.lock().take();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl LogReaderHandle {
    #[cfg(not(unix))]
    fn new() -> Self {
        Self {
            cancellation: PtyCancellation::new(),
        }
    }

    pub(super) fn cancel(&self) {
        self.cancellation.cancel();
    }
}

impl Drop for LogReaderHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(all(test, unix))]
impl LogReaderHandle {
    /// A handle with no cancellation pipe, for unit tests that only exercise handle bookkeeping.
    pub(super) fn test_dummy() -> Self {
        Self {
            cancellation: PtyCancellation {
                cancelled: Arc::new(AtomicBool::new(false)),
                cancel_write: Arc::new(Mutex::new(None)),
            },
        }
    }
}

#[cfg(all(test, unix))]
impl PtyHandles {
    /// Handles over a fresh PTY with no child attached, for unit tests that need a
    /// `RunningService` without spawning a process.
    pub(super) fn test_dummy() -> Result<Self, Error> {
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| Error::operation("failed to open test pty", err))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|err| Error::operation("failed to take test pty writer", err))?;
        let log_reader = LogReaderHandle::test_dummy();
        let (events_tx, _events_rx) = mpsc::channel(1);
        let (handles, _shutdown) = Self::new(
            "test".to_string(),
            RunId::new(1),
            events_tx,
            pair.master,
            writer,
            Arc::new(AtomicU32::new(0)),
            &log_reader,
        )?;
        Ok(handles)
    }
}

#[cfg(unix)]
struct PollingPtyReader {
    reader: Box<dyn Read + Send>,
    poll_read: FileDescriptor,
    cancel_read: FileDescriptor,
    cancelled: Arc<AtomicBool>,
    poller: PtyPoller,
}

#[cfg(unix)]
struct BorrowedRawFd(RawFd);

#[cfg(unix)]
impl AsRawFileDescriptor for BorrowedRawFd {
    fn as_raw_file_descriptor(&self) -> filedescriptor::RawFileDescriptor {
        self.0
    }
}

/// Enables readiness-based PTY writes and cancellable teardown.
///
/// Unix master descriptors share one open file description, so setting the returned duplicate to
/// nonblocking also covers the writer and reader duplicates.
///
/// The same descriptor observes write readiness for that terminal.
#[cfg(unix)]
fn configure_nonblocking_pty(
    master: &(dyn portable_pty::MasterPty + Send),
) -> Result<FileDescriptor, Error> {
    let pty_fd = master.as_raw_fd().ok_or_else(|| {
        Error::operation(
            "failed to access native pty",
            "master did not expose a raw fd",
        )
    })?;
    let mut descriptor = FileDescriptor::dup(&BorrowedRawFd(pty_fd))
        .map_err(|err| Error::operation("failed to clone pty control fd", err))?;
    descriptor
        .set_non_blocking(true)
        .map_err(|err| Error::operation("failed to make pty nonblocking", err))?;
    Ok(descriptor)
}

#[cfg(unix)]
impl PollingPtyReader {
    fn new(
        master: &(dyn portable_pty::MasterPty + Send),
        cancel_read: FileDescriptor,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, Error> {
        let pty_fd = master.as_raw_fd().ok_or_else(|| {
            Error::operation(
                "failed to access native pty",
                "master did not expose a raw fd",
            )
        })?;
        let poll_read = FileDescriptor::dup(&BorrowedRawFd(pty_fd))
            .map_err(|err| Error::operation("failed to clone pty poll fd", err))?;
        let reader = master
            .try_clone_reader()
            .map_err(|err| Error::operation("failed to clone pty reader", err))?;
        let poller =
            PtyPoller::new().map_err(|err| Error::operation("failed to create pty poller", err))?;
        Ok(Self {
            reader,
            poll_read,
            cancel_read,
            cancelled,
            poller,
        })
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<Option<NonZeroUsize>> {
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return Ok(None);
            }
            let mut fds = [
                pollfd {
                    fd: self.poll_read.as_raw_fd(),
                    events: POLL_EVENTS,
                    revents: 0,
                },
                pollfd {
                    fd: self.cancel_read.as_raw_fd(),
                    events: POLL_EVENTS,
                    revents: 0,
                },
            ];

            match self.poller.poll(&mut fds, None) {
                Ok(_) => {}
                Err(err) if poll_error_is_retryable(&err) => continue,
                Err(err) => return Err(err),
            }

            let mut events = fds.iter().map(|fd| fd.revents);
            let pty_events = events.next().unwrap_or_default();
            let cancel_events = events.next().unwrap_or_default();

            if self.cancelled.load(Ordering::Acquire) || cancel_events & POLL_EVENTS != 0 {
                return Ok(None);
            }

            if pty_events & POLL_EVENTS != 0 {
                match self.reader.read(buf) {
                    Ok(0) => return Ok(None),
                    Ok(n) => return Ok(NonZeroUsize::new(n)),
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                    Err(err) if err.raw_os_error() == Some(Errno::EIO as i32) => {
                        return Ok(None);
                    }
                    Err(err) => return Err(err),
                }
            }
        }
    }
}

#[cfg(test)]
fn active_log_readers() -> &'static Mutex<std::collections::HashSet<(ServiceID, RunId)>> {
    static ACTIVE: std::sync::OnceLock<Mutex<std::collections::HashSet<(ServiceID, RunId)>>> =
        std::sync::OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

// Only the Unix-gated scheduler tests query this, so gate it to `unix` as well; otherwise the
// Windows test build compiles it with no callers and flags it as dead code.
#[cfg(all(test, unix))]
pub(super) fn log_reader_active(service_id: &ServiceID, run_id: RunId) -> bool {
    active_log_readers()
        .lock()
        .contains(&(service_id.clone(), run_id))
}

#[cfg(test)]
struct ActiveLogReaderGuard {
    service_id: ServiceID,
    run_id: RunId,
}

#[cfg(test)]
impl ActiveLogReaderGuard {
    fn new(service_id: ServiceID, run_id: RunId) -> Self {
        active_log_readers()
            .lock()
            .insert((service_id.clone(), run_id));
        Self { service_id, run_id }
    }
}

#[cfg(test)]
impl Drop for ActiveLogReaderGuard {
    fn drop(&mut self) {
        active_log_readers()
            .lock()
            .remove(&(self.service_id.clone(), self.run_id));
    }
}

struct LogReaderArgs {
    service_id: ServiceID,
    run_id: RunId,
    sink: RunSink,
    reader: PtyOutputReader,
    write_queue: SharedPtyWriteQueue,
    events_tx: mpsc::Sender<ProcessEvent>,
    pty_rows: u16,
    pty_cols: u16,
    pty_size: Arc<AtomicU32>,
}

struct LogReaderFinishedGuard<'a> {
    events_tx: &'a mpsc::Sender<ProcessEvent>,
    service_id: &'a ServiceID,
    run_id: RunId,
}

impl Drop for LogReaderFinishedGuard<'_> {
    fn drop(&mut self) {
        let _ = self
            .events_tx
            .blocking_send(ProcessEvent::LogReaderFinished {
                service_id: self.service_id.clone(),
                run_id: self.run_id,
            });
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the PTY reader thread owns the terminal emulator and rate-limit state in one loop"
)]
fn spawn_log_reader_thread(args: LogReaderArgs) {
    thread::spawn(move || {
        #[derive(Clone)]
        struct PtyEventProxy {
            write_queue: SharedPtyWriteQueue,
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

                // Queue access is best-effort because this callback runs on the reader thread.
                // Contention or saturation drops the whole reply before any bytes are written.
                let Some(write_queue) = self.write_queue.try_lock() else {
                    return;
                };
                let Some(write_queue) = write_queue.as_ref() else {
                    return;
                };
                let _ = write_queue.try_send(PtyWrite::TerminalResponse(text.into_bytes()));
            }
        }

        fn emit_snapshot(
            term: &Term<PtyEventProxy>,
            rate: &mut RateLimit,
            sink: &RunSink,
            force: bool,
        ) {
            match rate.snapshot_decision(force, Instant::now()) {
                SnapshotDecision::Emit => {}
                SnapshotDecision::Warn => {
                    sink.append_log(
                        OutputStream::Stdout,
                        LogUpdateKind::Append,
                        "[micromux] interactive output rate-limited".to_string(),
                    );
                    return;
                }
                SnapshotDecision::Drop => return,
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

            sink.append_log(
                OutputStream::Stdout,
                LogUpdateKind::LiveSnapshot {
                    id: rate.snapshot_id(),
                },
                snapshot,
            );
            rate.record_snapshot_sent();
        }

        fn flush(line: &mut Vec<u8>, sink: &RunSink) {
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
            sink.append_log(OutputStream::Stdout, LogUpdateKind::Append, s);
            line.clear();
        }

        fn flush_bounded(line: &mut Vec<u8>, sink: &RunSink) {
            let split = bounded_line_split(line);
            let suffix = line.split_off(split);
            flush(line, sink);
            *line = suffix;
        }

        /// Emit a complete newline-terminated record, preserving blank/whitespace-only lines.
        ///
        /// Unlike [`flush`] (used for partial lines at EOF / the 16 KiB overflow guard), this
        /// emits the record even when empty so intentional blank lines are not silently dropped.
        /// `line` never contains the terminating newline bytes themselves.
        fn flush_record(line: &mut Vec<u8>, sink: &RunSink) {
            let s = String::from_utf8_lossy(line).to_string();
            sink.append_log(OutputStream::Stdout, LogUpdateKind::Append, s);
            line.clear();
        }

        fn finish_stream(
            snapshot_mode: bool,
            term: &Term<PtyEventProxy>,
            rate: &mut RateLimit,
            sink: &RunSink,
            line: &mut Vec<u8>,
        ) {
            if snapshot_mode {
                emit_snapshot(term, rate, sink, true);
            } else {
                flush(line, sink);
            }
        }

        let LogReaderArgs {
            service_id,
            run_id,
            sink,
            reader,
            write_queue,
            events_tx,
            pty_rows,
            pty_cols,
            pty_size,
        } = args;

        // The scheduler reserves this run ID until acknowledgement, so every thread exit path must
        // send one, including I/O errors and unwinding.
        let _finished = LogReaderFinishedGuard {
            events_tx: &events_tx,
            service_id: &service_id,
            run_id,
        };
        #[cfg(test)]
        let _active_reader = ActiveLogReaderGuard::new(service_id.clone(), run_id);

        let mut reader = reader;
        let mut buf = [0u8; 4096];
        let mut line: Vec<u8> = Vec::new();
        let mut scratch: Vec<u8> = Vec::new();
        let mut filter = AnsiFilter::new();
        let proxy = PtyEventProxy {
            write_queue,
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
        let mut snapshot_mode = false;
        let mut last_snapshot_at: Option<Instant> = None;
        let mut dirty = false;
        let mut last_size = 0u32;
        let mut last_alt_screen = false;
        // Tracks a pending CR so a following LF (i.e. a \r\n pair) does not emit a second,
        // spurious blank record after the \r already flushed the line.
        let mut prev_was_cr = false;
        // A CR seen at column 0 (empty line buffer) is ambiguous one byte early: an LF next makes
        // it the CRLF of a blank line, while content, EOF, or nothing makes it a cursor no-op that
        // must produce no record. Defer the blank until the next byte decides. BSD/macOS tty
        // drivers re-emit a CR around an ONLCR-expanded `\r\n` when output-queue backpressure
        // splits the write, so undeferred CRs inject spurious blank records under load.
        let mut pending_cr_blank = false;

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
                Ok(Some(n)) => n.get(),
                Ok(None) => break,
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        %service_id,
                        run_id = run_id.get(),
                        "failed to read service pty"
                    );
                    break;
                }
            };

            let Some(chunk) = buf.get(..n) else {
                continue;
            };

            for &b in chunk {
                let line_break_is_visible = !snapshot_mode && filter.captures_line_break();
                match b {
                    // \r and \n both terminate a line. A \r\n pair is coalesced (the \r flushes,
                    // the trailing \n is swallowed) so it produces one record, while a lone \n
                    // still flushes — preserving intentional blank lines.
                    //
                    // Records mirror what a terminal displays: a \r that arrives at column 0
                    // (empty line buffer) is a cursor no-op, not a line ending. It becomes a blank
                    // record only when an LF completes it (`pending_cr_blank`); content or EOF
                    // after it produces nothing. This absorbs the duplicated CR that BSD/macOS tty
                    // drivers emit around an ONLCR-expanded `\r\n` under load — whichever side of
                    // the pair the duplicate lands on — while keeping genuine blank lines intact.
                    b'\r' if line_break_is_visible => {
                        if line.is_empty() {
                            // Successive CRs collapse: after a CR the cursor is already at
                            // column 0, so returning it again cannot open another record.
                            if !prev_was_cr {
                                pending_cr_blank = true;
                            }
                        } else {
                            flush_record(&mut line, &sink);
                        }
                        prev_was_cr = true;
                    }
                    b'\n' if line_break_is_visible => {
                        if pending_cr_blank || !prev_was_cr {
                            flush_record(&mut line, &sink);
                        }
                        pending_cr_blank = false;
                        prev_was_cr = false;
                    }
                    _ => {
                        pending_cr_blank = false;
                        prev_was_cr = false;
                        if snapshot_mode {
                            scratch.clear();
                            let _ = filter.push(b, &mut scratch);
                        } else {
                            let boundary = filter.push(b, &mut line);
                            if boundary {
                                flush(&mut line, &sink);
                            }
                        }

                        if !snapshot_mode && line.len() >= PTY_LOG_LINE_MAX_BYTES {
                            flush_bounded(&mut line, &sink);
                        }
                    }
                }

                // Consume the byte in the mode that was active when its control sequence began.
                // The final byte of an alt-screen transition completes that sequence; switching
                // capture first would strand the line-mode ANSI filter in its intermediate state.
                processor.advance(&mut term, std::slice::from_ref(&b));
                let alt_screen = term.mode().contains(TermMode::ALT_SCREEN);
                if alt_screen != last_alt_screen {
                    last_alt_screen = alt_screen;
                    rate.set_alt_screen(alt_screen);
                    snapshot_mode = alt_screen;
                    dirty = alt_screen;
                    if alt_screen && !line.is_empty() {
                        flush_record(&mut line, &sink);
                    } else {
                        line.clear();
                    }
                    // The mode switch ends the current line record, so a CR seen before the flip
                    // must not carry over and swallow the next newline once line mode resumes.
                    prev_was_cr = false;
                    pending_cr_blank = false;
                }
                if snapshot_mode {
                    dirty = true;
                }
            }

            if snapshot_mode {
                let interval = Duration::from_millis(250);
                let now = Instant::now();
                let due = last_snapshot_at.is_none_or(|t| now.duration_since(t) >= interval);
                if dirty && due {
                    emit_snapshot(&term, &mut rate, &sink, false);
                    last_snapshot_at = Some(now);
                    dirty = false;
                }
            }
        }
        finish_stream(snapshot_mode, &term, &mut rate, &sink, &mut line);
    });
}

struct TerminationTaskArgs {
    service_id: ServiceID,
    run_id: RunId,
    events_tx: mpsc::Sender<ProcessEvent>,
    shutdown: CancellationToken,
    terminate: CancellationToken,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    pid: Option<u32>,
    process_group_leader_id: Option<i32>,
    stop_signal: crate::spec::StopSignal,
    #[cfg(unix)]
    leader_stamp: Option<crate::process_tree::ProcessStamp>,
    pty_shutdown: PtyShutdown,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    health_task: Option<tokio::task::JoinHandle<()>>,
    #[cfg(windows)]
    process_job: win32job::Job,
}

#[derive(Clone, Copy)]
struct TerminationTiming {
    force_kill_after: Duration,
    pty_hangup_after: Duration,
}

struct TerminationStart {
    kill_deadline: Option<tokio::time::Instant>,
    escalated: bool,
    pending_notification: Option<tokio::task::JoinHandle<()>>,
}

struct TerminationTarget {
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    pid: Option<u32>,
    process_group_leader_id: Option<i32>,
    /// Identity of the direct child at spawn time, anchoring pid-reuse-safe descendant
    /// sweeps.
    #[cfg(unix)]
    leader_stamp: Option<crate::process_tree::ProcessStamp>,
    /// Descendants observed while the leader was alive.
    ///
    /// The kernel reparents a process's children when it exits, so a walk rooted at a
    /// dead leader finds nothing. Retaining what earlier passes saw is the only way to
    /// still reach an escapee once the leader is gone.
    #[cfg(unix)]
    retained: Vec<crate::process_tree::Descendant>,
}

/// Maps the configured graceful-stop signal onto the OS signal to deliver.
#[cfg(unix)]
fn stop_signal_to_nix(signal: crate::spec::StopSignal) -> Signal {
    use crate::spec::StopSignal;
    match signal {
        StopSignal::Term => Signal::SIGTERM,
        StopSignal::Int => Signal::SIGINT,
        StopSignal::Hup => Signal::SIGHUP,
        StopSignal::Quit => Signal::SIGQUIT,
        StopSignal::Usr1 => Signal::SIGUSR1,
        StopSignal::Usr2 => Signal::SIGUSR2,
    }
}

impl TerminationTarget {
    fn new(
        killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
        pid: Option<u32>,
        process_group_leader_id: Option<i32>,
        #[cfg(unix)] leader_stamp: Option<crate::process_tree::ProcessStamp>,
    ) -> Self {
        Self {
            killer,
            pid,
            process_group_leader_id,
            #[cfg(unix)]
            leader_stamp,
            #[cfg(unix)]
            retained: Vec::new(),
        }
    }

    async fn request(
        &mut self,
        events_tx: &mpsc::Sender<ProcessEvent>,
        service_id: &ServiceID,
        run_id: RunId,
        force_kill_after: Duration,
        stop_signal: crate::spec::StopSignal,
    ) -> TerminationStart {
        #[cfg(not(unix))]
        let _ = (force_kill_after, stop_signal);

        #[cfg(unix)]
        tracing::info!(pid = self.pid, service_id, %stop_signal, "killing process");
        // Windows terminates through the backend, so naming a signal here would report
        // one that is never delivered.
        #[cfg(not(unix))]
        tracing::info!(pid = self.pid, service_id, "terminating process");

        #[cfg(unix)]
        let (kill_deadline, escalated) = {
            // Snapshot before signaling: this is the last moment the leader is
            // reliably alive, and once it exits its descendants reparent away and
            // become undiscoverable.
            self.refresh_retained().await;
            let signal = stop_signal_to_nix(stop_signal);
            let delivery = self.signal_leader(signal);
            if delivery == LeaderSignal::Missed {
                self.kill_with_backend();
            }
            signal_descendants(
                self.retained.clone(),
                self.leader_group(),
                delivery == LeaderSignal::Group,
                signal,
            )
            .await;
            let now = tokio::time::Instant::now();
            (
                Some(now.checked_add(force_kill_after).unwrap_or(now)),
                false,
            )
        };

        #[cfg(not(unix))]
        let (kill_deadline, escalated) = {
            let _ = self.process_group_leader_id;
            self.kill_with_backend();
            (None, true)
        };

        // Signal delivery establishes the stop deadline. If notification outlives the short inline
        // wait, finish it independently so escalation continues while `Killed` remains ordered
        // before `Exited`.
        let event = ProcessEvent::Killed {
            service_id: service_id.clone(),
            run_id,
        };
        let pending_notification = if tokio::time::timeout(
            TERMINATION_EVENT_NOTIFY_TIMEOUT,
            events_tx.send(event.clone()),
        )
        .await
        .is_err()
        {
            let events_tx = events_tx.clone();
            Some(tokio::spawn(async move {
                let _ = events_tx.send(event).await;
            }))
        } else {
            None
        };
        TerminationStart {
            kill_deadline,
            escalated,
            pending_notification,
        }
    }

    /// Force-kills the leader, its group, and — on Unix — every descendant a
    /// synchronous sweep can reach.
    ///
    /// Used by [`SpawnedChildGuard`], whose `Drop` may run outside a runtime and
    /// therefore cannot await: the walk and expansion run inline on the caller's
    /// thread. A child that fails setup has been executing since spawn and may
    /// already have forked or escaped the group, so a non-sweeping shortcut here
    /// would leak those descendants past the startup error. Startup failure is rare
    /// enough that blocking the caller for the scans is acceptable; no verification
    /// tail exists on this path.
    fn force_kill(&mut self) {
        #[cfg(unix)]
        {
            // Walk before the kill: the walk needs the leader alive to be trustworthy.
            let retained = self
                .leader_stamp
                .map(crate::process_tree::descendants)
                .unwrap_or_default();
            if self.signal_leader(Signal::SIGKILL) == LeaderSignal::Missed {
                self.kill_with_backend();
            }
            let mut groups: std::collections::HashSet<u32> = retained
                .iter()
                .filter_map(|descendant| descendant.process_group)
                .collect();
            groups.extend(self.leader_group());
            let stamps: Vec<crate::process_tree::ProcessStamp> =
                retained.iter().map(|descendant| descendant.stamp).collect();
            for survivor in crate::process_tree::expand_survivors(&stamps, &groups) {
                let _ = crate::process_tree::send_signal(survivor, Signal::SIGKILL);
            }
        }

        #[cfg(not(unix))]
        {
            self.kill_with_backend();
        }
    }

    /// Force-kills the leader, its group, and every descendant the expansion reaches.
    ///
    /// Converges with [`reap_retained`] on the same final operation: expand the
    /// retained stamps and recorded groups, then SIGKILL the expanded set. The
    /// expansion is what recovers workers forked since the snapshot whose forker has
    /// already died — the fresh walk from the still-live leader cannot see them,
    /// because they reparented away; only their recorded group still names them.
    ///
    /// Consumes the retained set, returning the stamps that were signaled so
    /// [`verify_reaped`] can retry failed deliveries and report leaks.
    #[cfg(unix)]
    async fn force_kill_swept(&mut self) -> Vec<crate::process_tree::ProcessStamp> {
        // A last walk while the leader may still be alive picks up anything forked
        // since the graceful pass that is still reachable through it.
        self.refresh_retained().await;
        let delivery = self.signal_leader(Signal::SIGKILL);
        if delivery == LeaderSignal::Missed {
            self.kill_with_backend();
        }
        let retained = std::mem::take(&mut self.retained);
        let mut groups: std::collections::HashSet<u32> = retained
            .iter()
            .filter_map(|descendant| descendant.process_group)
            .collect();
        // The leader's group joins unconditionally, closing the narrow window where a
        // member forked between the walk above and the killpg's delivery.
        groups.extend(self.leader_group());
        let stamps = retained.iter().map(|descendant| descendant.stamp).collect();
        // Members of the leader's own group need no individual delivery — the killpg
        // above already reached them and SIGKILL cannot be blocked — but re-signaling
        // them by stamp is harmless, so the expanded set is killed uniformly.
        let survivors = expand_survivors(stamps, groups).await;
        signal_stamps(survivors.clone(), Signal::SIGKILL).await;
        survivors
    }

    /// The leader's process-group id, when it fits the descendant records' encoding.
    #[cfg(unix)]
    fn leader_group(&self) -> Option<u32> {
        self.process_group_leader_id
            .and_then(|pgid| u32::try_from(pgid).ok())
    }

    /// Falls back to portable-pty's killer when direct signalling failed.
    ///
    /// On Unix the backend delivers `SIGHUP` — portable-pty's `ProcessSignaller` — not
    /// `SIGKILL`, so despite its force-kill callsites this is the weakest rung: a
    /// process that ignores hangups survives it, and only the PTY-teardown ladder
    /// remains. On Windows it terminates the process through the backend handle.
    fn kill_with_backend(&mut self) {
        if let Err(err) = self.killer.kill() {
            #[cfg(unix)]
            if err.raw_os_error() == Some(Errno::ESRCH as i32) {
                tracing::debug!(pid = self.pid, "process exited before backend kill");
                return;
            }
            tracing::warn!(?err, pid = self.pid, "failed to kill process");
        }
    }

    /// Walks the descendant tree and merges the result into the retained set.
    ///
    /// The walk reads the whole process table, so it runs on the blocking pool: on a
    /// busy host it costs milliseconds of syscalls, which would otherwise stall a
    /// runtime worker and delay the force-kill and PTY-hangup deadlines.
    #[cfg(unix)]
    async fn refresh_retained(&mut self) {
        let Some(leader) = self.leader_stamp else {
            return;
        };
        let observed =
            tokio::task::spawn_blocking(move || crate::process_tree::descendants(leader))
                .await
                .unwrap_or_default();
        for descendant in observed {
            // Dedup by identity, not by the whole record: a process that changed its
            // group between walks must not appear twice, or the sweep would deliver the
            // stop signal to it twice. The newest group observation wins so the
            // killpg-coverage skip stays accurate.
            if let Some(existing) = self
                .retained
                .iter_mut()
                .find(|retained| retained.stamp == descendant.stamp)
            {
                existing.process_group = descendant.process_group;
            } else {
                self.retained.push(descendant);
            }
        }
    }

    /// Signals the leader's process group, falling back to the leader alone.
    ///
    /// The pid is deliberately not re-verified first. A stamp-based liveness check would
    /// be wrong here: a leader that exited but is not yet reaped is a zombie, which
    /// reads as "not alive" while still holding its pid, so the check would skip
    /// `killpg` and leave the rest of the group unsignalled during an ordinary stop —
    /// and neither platform can portably distinguish zombie from reaped.
    ///
    /// The residual is a narrow window: the child's `wait` reaps concurrently on the
    /// blocking pool, so between a descendant walk and this call the pid could in
    /// principle be freed and recycled. Both platforms allocate pids cyclically, so
    /// reuse within milliseconds requires a full pid-space wrap; the risk is accepted,
    /// as it is by every process-tree killer without kernel containment.
    #[cfg(unix)]
    fn signal_leader(&self, signal: Signal) -> LeaderSignal {
        if let Some(pgid) = self.process_group_leader_id {
            match nix::sys::signal::killpg(Pid::from_raw(pgid), signal) {
                Ok(()) => return LeaderSignal::Group,
                Err(Errno::ESRCH) => {
                    tracing::debug!(?signal, pgid, "process group exited before signal delivery");
                }
                Err(err) => {
                    tracing::warn!(?err, ?signal, pgid, "failed to signal process group");
                }
            }
        }
        if let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok()) {
            match nix::sys::signal::kill(Pid::from_raw(pid), signal) {
                Ok(()) => return LeaderSignal::LeaderOnly,
                Err(Errno::ESRCH) => {
                    tracing::debug!(?signal, pid, "process exited before signal delivery");
                }
                Err(err) => {
                    tracing::warn!(?err, ?signal, pid, "failed to signal process");
                }
            }
        }
        LeaderSignal::Missed
    }
}

/// How far a leader-directed signal reached.
///
/// The distinction matters downstream: only a [`LeaderSignal::Group`] delivery lets
/// the descendant sweep skip in-group members — after a leader-only fallback those
/// members have received nothing yet.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum LeaderSignal {
    /// `killpg` reached the leader's whole process group.
    Group,
    /// Only the leader itself was reachable; its group members were not signaled.
    LeaderOnly,
    /// Neither the group nor the leader could be signaled.
    Missed,
}

/// Signals every retained descendant that a `killpg` did not already cover.
///
/// `group_signaled` reports whether the leader's whole group received this signal.
/// When it did, members of that group are skipped: re-delivering is not harmless for
/// the configurable stop signals, since a second `SIGINT` is widely treated as "abort
/// now, skip cleanup" — the opposite of a graceful stop.
///
/// Runs on the blocking pool: each delivery is an identity re-check plus a syscall,
/// and a large tree would otherwise occupy an async worker. A pid recycled since the
/// snapshot is skipped rather than signaled.
#[cfg(unix)]
async fn signal_descendants(
    descendants: Vec<crate::process_tree::Descendant>,
    leader_group: Option<u32>,
    group_signaled: bool,
    signal: Signal,
) {
    if descendants.is_empty() {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || {
        for descendant in &descendants {
            if group_signaled && leader_group.is_some() && descendant.process_group == leader_group
            {
                continue;
            }
            let _ = crate::process_tree::send_signal(descendant.stamp, signal);
        }
    })
    .await;
}

struct SpawnedChildGuard {
    target: Option<TerminationTarget>,
}

impl SpawnedChildGuard {
    fn new(
        killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
        pid: Option<u32>,
        process_group_leader_id: Option<i32>,
        #[cfg(unix)] leader_stamp: Option<crate::process_tree::ProcessStamp>,
    ) -> Self {
        Self {
            target: Some(TerminationTarget::new(
                killer,
                pid,
                process_group_leader_id,
                #[cfg(unix)]
                leader_stamp,
            )),
        }
    }

    fn disarm(&mut self) {
        self.target = None;
    }
}

impl Drop for SpawnedChildGuard {
    fn drop(&mut self) {
        if let Some(mut target) = self.target.take() {
            target.force_kill();
        }
    }
}

fn exit_code_from_wait(
    result: Result<io::Result<portable_pty::ExitStatus>, tokio::task::JoinError>,
    service_id: &ServiceID,
    run_id: RunId,
    pid: Option<u32>,
) -> i32 {
    match result {
        Ok(Ok(status)) => i32::try_from(status.exit_code()).unwrap_or(i32::MAX),
        Ok(Err(err)) => {
            tracing::error!(
                ?err,
                ?pid,
                %service_id,
                run_id = run_id.get(),
                "failed to wait for process"
            );
            -1
        }
        Err(err) => {
            tracing::error!(
                ?err,
                ?pid,
                %service_id,
                run_id = run_id.get(),
                "process wait task failed"
            );
            -1
        }
    }
}

fn spawn_termination_task(args: TerminationTaskArgs, force_kill_after: Duration) {
    spawn_termination_task_with_timing(
        args,
        TerminationTiming {
            force_kill_after,
            pty_hangup_after: PTY_HANGUP_AFTER_ESCALATION,
        },
    );
}

async fn stop_health_task(
    service_id: &ServiceID,
    health_task: &mut Option<tokio::task::JoinHandle<()>>,
) {
    if let Some(mut task) = health_task.take()
        && tokio::time::timeout(HEALTH_TASK_STOP_TIMEOUT, &mut task)
            .await
            .is_err()
    {
        tracing::warn!(%service_id, "timed out stopping healthcheck task");
        task.abort();
        let _ = task.await;
    }
}

/// Delay between post-SIGKILL verification rounds, long enough for the kernel to
/// reparent and reap killed orphans.
#[cfg(unix)]
const KILL_VERIFICATION_INTERVAL: Duration = Duration::from_millis(200);

/// Poll interval while waiting out the remaining grace period for retained descendants.
#[cfg(unix)]
const REAP_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Retains only the stamps that still name live processes, on the blocking pool.
///
/// Each check is a process-table read; a large tree would otherwise occupy an async
/// worker with synchronous syscalls.
#[cfg(unix)]
async fn retain_alive(
    survivors: Vec<crate::process_tree::ProcessStamp>,
) -> Vec<crate::process_tree::ProcessStamp> {
    tokio::task::spawn_blocking(move || {
        let mut survivors = survivors;
        survivors.retain(|&stamp| crate::process_tree::is_alive(stamp));
        survivors
    })
    .await
    .unwrap_or_default()
}

/// Expands surviving stamps to descendants forked since the snapshot, on the blocking
/// pool. See [`crate::process_tree::expand_survivors`].
#[cfg(unix)]
async fn expand_survivors(
    survivors: Vec<crate::process_tree::ProcessStamp>,
    groups: std::collections::HashSet<u32>,
) -> Vec<crate::process_tree::ProcessStamp> {
    tokio::task::spawn_blocking(move || crate::process_tree::expand_survivors(&survivors, &groups))
        .await
        .unwrap_or_default()
}

/// Delivers `signal` to each stamp, on the blocking pool.
#[cfg(unix)]
async fn signal_stamps(survivors: Vec<crate::process_tree::ProcessStamp>, signal: Signal) {
    let _ = tokio::task::spawn_blocking(move || {
        for &stamp in &survivors {
            let _ = crate::process_tree::send_signal(stamp, signal);
        }
    })
    .await;
}

/// Grants retained descendants the rest of the grace window, then force-kills survivors.
///
/// This runs **before** the service's `Exited` event is published: `Exited` is what
/// lets the scheduler start a replacement or finish a shutdown drain, so the lethal
/// signal must already be delivered by then — a restart must not overlap live escapees,
/// and a runtime teardown must not outrun the kill.
///
/// `kill_at` is the graceful-stop deadline from the original request. Descendants that
/// already exited are noticed immediately, so the common case — the whole group died
/// with the graceful signal — adds no delay beyond one expansion scan.
///
/// `leader_group` joins the recorded groups even when the retained set is empty: a
/// single-process leader whose stop-signal handler forks an in-group helper and exits
/// leaves nothing in the snapshot, and only group membership can still find the
/// helper. The scan this costs on every requested termination is milliseconds on the
/// blocking pool.
///
/// Expansion discoveries share the original deadline rather than being killed on
/// sight: some received the graceful signal (forked between the snapshot and the
/// `killpg`), and the rest may be performing the leader's shutdown work, so they are
/// polled like the retained stamps until the grace runs out. Only an expansion that
/// finds nothing ends the reap early, which keeps clean stops free of added latency.
///
/// Returns the stamps that were force-killed, for [`verify_reaped`].
#[cfg(unix)]
async fn reap_retained(
    service_id: &ServiceID,
    run_id: RunId,
    retained: Vec<crate::process_tree::Descendant>,
    leader_group: Option<u32>,
    kill_at: Option<tokio::time::Instant>,
) -> Vec<crate::process_tree::ProcessStamp> {
    let mut groups: std::collections::HashSet<u32> = retained
        .iter()
        .filter_map(|descendant| descendant.process_group)
        .collect();
    groups.extend(leader_group);
    if retained.is_empty() && groups.is_empty() {
        return Vec::new();
    }
    let mut survivors: Vec<crate::process_tree::ProcessStamp> = retained
        .into_iter()
        .map(|descendant| descendant.stamp)
        .collect();
    // Wait out the remaining grace. The known stamps draining is not the end: an
    // expansion may still discover group members or fresh forks, and those inherit
    // the same deadline instead of being killed mid-grace. Only an empty expansion
    // leaves early.
    if let Some(deadline) = kill_at {
        loop {
            survivors = retain_alive(survivors).await;
            if survivors.is_empty() {
                survivors = expand_survivors(survivors, groups.clone()).await;
                if survivors.is_empty() {
                    return Vec::new();
                }
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            tokio::time::sleep(REAP_POLL_INTERVAL.min(deadline - now)).await;
        }
    } else {
        survivors = retain_alive(survivors).await;
    }
    // The snapshot aged while the grace elapsed: an escapee may have forked workers
    // the stamps do not name. One final expansion recovers them — live descendants of
    // the surviving roots, plus members of the recorded process groups even when the
    // forker itself is already gone. Forks racing this scan remain the documented
    // best-effort residual.
    survivors = expand_survivors(survivors, groups).await;
    if survivors.is_empty() {
        return Vec::new();
    }
    tracing::debug!(
        count = survivors.len(),
        %service_id,
        run_id = run_id.get(),
        "descendants outlived the service; forcing termination"
    );
    signal_stamps(survivors.clone(), Signal::SIGKILL).await;
    survivors
}

/// Re-attempts failed SIGKILL deliveries, then reports descendants that remain.
///
/// Runs after `Exited` on a best-effort basis: the kill itself already happened in
/// [`reap_retained`] or the escalation arm, so only the retry rounds and the leak
/// warning are lost if the runtime tears down first. Verification is confined to the
/// stamps that were signaled: [`reap_retained`]'s expansion already recovered forks
/// since the snapshot, and a `SIGKILL`ed process cannot fork again, so anything still
/// missing here raced the final scan and stays within the documented residual.
#[cfg(unix)]
async fn verify_reaped(
    service_id: &ServiceID,
    run_id: RunId,
    mut survivors: Vec<crate::process_tree::ProcessStamp>,
) {
    for _ in 0..2 {
        tokio::time::sleep(KILL_VERIFICATION_INTERVAL).await;
        survivors = retain_alive(survivors).await;
        if survivors.is_empty() {
            return;
        }
        signal_stamps(survivors.clone(), Signal::SIGKILL).await;
    }
    tokio::time::sleep(KILL_VERIFICATION_INTERVAL).await;
    survivors = retain_alive(survivors).await;
    if !survivors.is_empty() {
        let pids: Vec<u32> = survivors.iter().map(|stamp| stamp.pid).collect();
        tracing::warn!(
            ?pids,
            %service_id,
            run_id = run_id.get(),
            "descendants survived forced termination"
        );
    }
}

struct FinishChildExitArgs<'a> {
    wait_result: Result<io::Result<portable_pty::ExitStatus>, tokio::task::JoinError>,
    service_id: &'a ServiceID,
    run_id: RunId,
    pid: Option<u32>,
    events_tx: &'a mpsc::Sender<ProcessEvent>,
    health_task: &'a mut Option<tokio::task::JoinHandle<()>>,
    pending_killed_notification: Option<tokio::task::JoinHandle<()>>,
}

/// Publishes the exit event once the child wait completes, after the health task and
/// any pending killed notification wind down.
async fn finish_child_exit(args: FinishChildExitArgs<'_>) {
    let FinishChildExitArgs {
        wait_result,
        service_id,
        run_id,
        pid,
        events_tx,
        health_task,
        pending_killed_notification,
    } = args;
    stop_health_task(service_id, health_task).await;
    if let Some(notification) = pending_killed_notification {
        let _ = notification.await;
    }
    let code = exit_code_from_wait(wait_result, service_id, run_id, pid);
    let event = ProcessEvent::Exited {
        service_id: service_id.clone(),
        run_id,
        exit_code: code,
    };
    let _ = events_tx.send(event).await;
}

/// Closes every pty master endpoint when escalation leaves the child unreaped.
///
/// A descendant in another session can retain the slave after process-group
/// escalation; without the hangup the terminal could keep the original child's wait
/// from completing.
fn hangup_unreaped_pty(
    pid: Option<u32>,
    service_id: &ServiceID,
    run_id: RunId,
    pty_shutdown: &mut Option<PtyShutdown>,
) {
    tracing::warn!(
        ?pid,
        %service_id,
        run_id = run_id.get(),
        "termination escalated but the process is still unreaped; closing its pty"
    );
    if let Some(shutdown) = pty_shutdown.take() {
        shutdown.close();
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the escalation ladder is one state machine; its arms share deadlines and \
              cancellation state that splitting further would have to thread by hand"
)]
fn spawn_termination_task_with_timing(args: TerminationTaskArgs, timing: TerminationTiming) {
    tokio::spawn(async move {
        let TerminationTaskArgs {
            service_id,
            run_id,
            events_tx,
            shutdown,
            terminate,
            pid,
            process_group_leader_id,
            stop_signal,
            #[cfg(unix)]
            leader_stamp,
            pty_shutdown,
            killer,
            mut child,
            mut health_task,
            #[cfg(windows)]
            process_job,
        } = args;
        #[cfg(windows)]
        let _process_job = process_job;

        let mut target = TerminationTarget::new(
            killer,
            pid,
            process_group_leader_id,
            #[cfg(unix)]
            leader_stamp,
        );
        // The blocking-pool thread remains occupied until the child wait completes. Forced
        // termination may not unblock it if the OS is still tearing the process down.
        let mut wait_handle = tokio::task::spawn_blocking(move || child.wait());
        let mut termination_started = false;
        let mut termination_escalated = false;
        let mut kill_deadline: Option<tokio::time::Instant> = None;
        let mut pty_hangup_deadline: Option<tokio::time::Instant> = None;
        let mut pending_killed_notification = None;
        let mut pty_shutdown = Some(pty_shutdown);
        // Stamps whose deaths still need best-effort confirmation after `Exited`.
        #[cfg(unix)]
        let mut to_verify: Vec<crate::process_tree::ProcessStamp> = Vec::new();
        loop {
            tokio::select! {
                biased;
                res = &mut wait_handle => {
                    terminate.cancel();
                    // The kill must land before `Exited`: that event frees the scheduler
                    // to start a replacement or finish a shutdown drain, and neither may
                    // outrun live escapees. The writer is released first so the grace
                    // wait cannot pin PTY input meanwhile. Extending — not replacing —
                    // `to_verify` preserves the escalation arm's stamps for the
                    // verification tail; after escalation the retained set is empty and
                    // this reap contributes nothing further.
                    #[cfg(unix)]
                    if termination_started {
                        drop(pty_shutdown.take());
                        let remaining = std::mem::take(&mut target.retained);
                        let reaped = reap_retained(
                            &service_id,
                            run_id,
                            remaining,
                            target.leader_group(),
                            kill_deadline,
                        )
                        .await;
                        to_verify.extend(reaped);
                    }
                    finish_child_exit(FinishChildExitArgs {
                        wait_result: res,
                        service_id: &service_id,
                        run_id,
                        pid,
                        events_tx: &events_tx,
                        health_task: &mut health_task,
                        pending_killed_notification: pending_killed_notification.take(),
                    }).await;
                    break;
                }
                () = async {
                    tokio::select! {
                        () = shutdown.cancelled() => {}
                        () = terminate.cancelled() => {}
                    }
                }, if !termination_started => {
                    let started = target.request(
                        &events_tx,
                        &service_id,
                        run_id,
                        timing.force_kill_after,
                        stop_signal,
                    ).await;
                    kill_deadline = started.kill_deadline;
                    termination_escalated = started.escalated;
                    pending_killed_notification = started.pending_notification;
                    pty_hangup_deadline = started
                        .escalated
                        .then(|| tokio::time::Instant::now() + timing.pty_hangup_after);
                    termination_started = true;
                }
                () = async {
                    match kill_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                }, if termination_started && !termination_escalated => {
                    // On Unix, target the process group plus any descendants that escaped
                    // it so they cannot survive the leader and keep its PTY open.
                    #[cfg(unix)]
                    {
                        to_verify = target.force_kill_swept().await;
                    }
                    #[cfg(not(unix))]
                    target.force_kill();
                    termination_escalated = true;
                    pty_hangup_deadline =
                        Some(tokio::time::Instant::now() + timing.pty_hangup_after);
                }
                () = async {
                    match pty_hangup_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                }, if pty_hangup_deadline.is_some() => {
                    hangup_unreaped_pty(pid, &service_id, run_id, &mut pty_shutdown);
                    pty_hangup_deadline = None;
                }
            }
        }

        // Best-effort tail: the kills themselves already landed before `Exited`, so a
        // runtime teardown cutting this short loses only the retry rounds and the leak
        // warning.
        #[cfg(unix)]
        if !to_verify.is_empty() {
            verify_reaped(&service_id, run_id, to_verify).await;
        }
    });
}

#[expect(
    clippy::too_many_lines,
    reason = "process startup wires PTY, reader, waiter, and ownership guards in one fallible path"
)]
pub(super) fn start_service_with_pty_size(
    params: StartServiceParams<'_>,
) -> Result<StartedPty, Error> {
    use portable_pty::{CommandBuilder, PtySize};

    let StartServiceParams {
        service,
        run_id,
        sink,
        events_tx,
        shutdown,
        terminate,
        pty_size,
    } = params;
    let service_id = service.id.clone();
    let Some((prog, args)) = service.spec.command.split_first() else {
        return Err(Error::EmptyCommand);
    };
    let working_dir = service
        .spawn_working_directory()
        .map_err(|err| Error::operation("failed to resolve anchored working directory", err))?;

    let env_vars = env_vars_for_service(service);
    let env_vars = {
        let mut env_vars = env_vars;
        env_vars
            .entry("TERM".to_string())
            .or_insert_with(|| "xterm-256color".to_string());
        env_vars
    };

    let mut env_keys = env_vars.keys().map(String::as_str).collect::<Vec<_>>();
    env_keys.sort_unstable();
    // Values may contain secrets from env files; log only the key names.
    tracing::info!(
        service_id,
        prog,
        args_count = args.len(),
        ?env_keys,
        "start service"
    );

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: pty_size.rows,
            cols: pty_size.cols,
            pixel_width: pty_size.pixel_width,
            pixel_height: pty_size.pixel_height,
        })
        .map_err(|err| Error::operation("failed to open pty", err))?;

    let mut cmd = CommandBuilder::new(prog);
    cmd.args(args);
    if let Some(dir) = &working_dir {
        cmd.cwd(dir.as_path());
    }
    for (k, v) in &env_vars {
        cmd.env(k, v);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|err| Error::operation("failed to spawn in pty", err))?;

    let pid = child.process_id();
    let killer = child.clone_killer();

    // The pty slave spawn calls `setsid()` in the child before exec, making it a session
    // and process-group leader, so its pgid equals its pid. Deriving the group from the
    // pid avoids `tcgetpgrp` on the master, which reports a sentinel naming no existing
    // group once the child exits before the query.
    #[cfg(unix)]
    let process_group_leader = pid.and_then(|pid| i32::try_from(pid).ok());
    #[cfg(not(unix))]
    let process_group_leader = None;

    // Record the child's identity while its pid is guaranteed fresh, so later descendant
    // sweeps can detect pid reuse instead of walking a stranger's process tree.
    #[cfg(unix)]
    let leader_stamp = pid.and_then(crate::process_tree::stamp);
    // A missing stamp disables the descendant sweep for this run's whole lifetime; the
    // process-group kill and PTY teardown remain, but escapees would leak silently, so
    // the degradation must be visible.
    #[cfg(unix)]
    if leader_stamp.is_none() {
        tracing::warn!(
            ?pid,
            service_id,
            "cannot identify the spawned child; descendant sweep disabled for this run"
        );
    }

    let mut child_guard = SpawnedChildGuard::new(
        child.clone_killer(),
        pid,
        process_group_leader,
        #[cfg(unix)]
        leader_stamp,
    );

    #[cfg(windows)]
    let process_job = {
        let handle = child.as_raw_handle().ok_or_else(|| {
            Error::operation(
                "failed to contain service process",
                "child did not expose a process handle",
            )
        })?;
        crate::windows_job::attach_kill_on_close(handle as isize)
            .map_err(|err| Error::operation("failed to contain service process", err))?
    };

    let (reader, log_reader) = PtyOutputReader::new(pair.master.as_ref())?;

    let writer = pair
        .master
        .take_writer()
        .map_err(|err| Error::operation("failed to take pty writer", err))?;

    let size = Arc::new(AtomicU32::new(
        (u32::from(pty_size.rows) << 16) | u32::from(pty_size.cols),
    ));
    let (handles, pty_shutdown) = PtyHandles::new(
        service_id.clone(),
        run_id,
        events_tx.clone(),
        pair.master,
        writer,
        size.clone(),
        &log_reader,
    )?;

    spawn_log_reader_thread(LogReaderArgs {
        service_id: service_id.clone(),
        run_id,
        sink: sink.clone(),
        reader,
        write_queue: handles.write_queue.clone(),
        events_tx: events_tx.clone(),
        pty_rows: pty_size.rows,
        pty_cols: pty_size.cols,
        pty_size: size.clone(),
    });

    let health_task = service.spec.healthcheck.clone().map(|health_check| {
        let service_id = service_id.clone();
        let working_dir = working_dir.clone();
        let environment: std::collections::HashMap<String, String> = service
            .spec
            .environment
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let events_tx = events_tx.clone();
        let shutdown = shutdown.clone();
        let terminate = terminate.clone();
        tokio::spawn(async move {
            health_check::run_loop(
                health_check,
                health_check::RunLoopParams {
                    service_id,
                    run_id,
                    sink,
                    working_dir,
                    environment,
                    events_tx,
                    shutdown,
                    terminate,
                },
            )
            .await;
        })
    });

    spawn_termination_task(
        TerminationTaskArgs {
            service_id: service_id.clone(),
            run_id,
            events_tx: events_tx.clone(),
            shutdown: shutdown.clone(),
            terminate: terminate.clone(),
            killer,
            pid,
            process_group_leader_id: process_group_leader,
            stop_signal: service.spec.stop_signal,
            #[cfg(unix)]
            leader_stamp,
            pty_shutdown,
            child,
            health_task,
            #[cfg(windows)]
            process_job,
        },
        service.spec.stop_grace_period,
    );
    child_guard.disarm();

    Ok(StartedPty {
        pid,
        handles,
        log_reader,
    })
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use similar_asserts::assert_eq;

    #[test]
    fn log_reader_finished_guard_reports_completion_on_drop() {
        let (events_tx, mut events_rx) = mpsc::channel(1);
        let service_id = "svc".to_string();
        let run_id = RunId::new(7);

        {
            let _finished = LogReaderFinishedGuard {
                events_tx: &events_tx,
                service_id: &service_id,
                run_id,
            };
        }

        assert_matches!(
            events_rx.blocking_recv(),
            Some(ProcessEvent::LogReaderFinished {
                service_id: finished_service_id,
                run_id: finished_run_id,
            }) if finished_service_id == service_id && finished_run_id == run_id
        );
    }

    #[test]
    fn ansi_filter_recovers_from_unterminated_string_sequences() {
        for introducer in *b"]P^_" {
            let mut filter = AnsiFilter::new();
            let mut output = Vec::new();
            for byte in [0x1b, introducer]
                .into_iter()
                .chain(std::iter::repeat_n(
                    b'x',
                    ANSI_SEQUENCE_PAYLOAD_MAX_BYTES + 1,
                ))
                .chain(*b"visible")
            {
                let _ = filter.push(byte, &mut output);
            }
            assert_eq!(output, b"visible");
        }
    }

    #[test]
    fn ansi_filter_recognizes_string_terminators_across_reads() {
        let mut filter = AnsiFilter::new();
        let mut output = Vec::new();
        for byte in b"\x1b]title\x1b" {
            let _ = filter.push(*byte, &mut output);
        }
        for byte in b"\\visible" {
            let _ = filter.push(*byte, &mut output);
        }

        assert_eq!(output, b"visible");

        let mut filter = AnsiFilter::new();
        let mut output = Vec::new();
        for byte in b"\x1b]title\x9cvisible" {
            let _ = filter.push(*byte, &mut output);
        }
        assert_eq!(output, b"visible");

        let mut filter = AnsiFilter::new();
        let mut output = Vec::new();
        for byte in b"\x1b]"
            .iter()
            .copied()
            .chain(std::iter::repeat_n(b'x', ANSI_SEQUENCE_PAYLOAD_MAX_BYTES))
            .chain(*b"\x1b\\visible")
        {
            let _ = filter.push(byte, &mut output);
        }
        assert_eq!(output, b"visible");
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use crate::scheduler::input::{InputBudget, PtyInputKind, PtyInputPrepareError};
        use crate::{MAX_PTY_INPUT_BATCH_BYTES, MAX_PTY_PASTE_BYTES};
        use color_eyre::eyre::{self, OptionExt as _};
        use similar_asserts::assert_eq;
        use std::fs;

        #[derive(Clone, Debug)]
        struct CountingKiller {
            calls: Arc<std::sync::atomic::AtomicUsize>,
        }

        impl portable_pty::ChildKiller for CountingKiller {
            fn kill(&mut self) -> io::Result<()> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }

            fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
                Box::new(self.clone())
            }
        }

        struct ExitOnDropWriter {
            writer: Option<Box<dyn Write + Send>>,
            exited: Option<std::sync::mpsc::Sender<()>>,
            would_block: Option<std::sync::mpsc::Sender<()>>,
        }

        impl Write for ExitOnDropWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                let Some(writer) = self.writer.as_mut() else {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "test PTY writer was already dropped",
                    ));
                };
                let result = writer.write(buf);
                if matches!(&result, Err(err) if err.kind() == io::ErrorKind::WouldBlock)
                    && let Some(would_block) = self.would_block.take()
                {
                    let _ = would_block.send(());
                }
                result
            }

            fn flush(&mut self) -> io::Result<()> {
                let Some(writer) = self.writer.as_mut() else {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "test PTY writer was already dropped",
                    ));
                };
                writer.flush()
            }
        }

        impl Drop for ExitOnDropWriter {
            fn drop(&mut self) {
                drop(self.writer.take());
                if let Some(exited) = self.exited.take() {
                    let _ = exited.send(());
                }
            }
        }

        struct WaitingChild {
            exited: Mutex<std::sync::mpsc::Receiver<()>>,
            kill_calls: Arc<std::sync::atomic::AtomicUsize>,
        }

        impl std::fmt::Debug for WaitingChild {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter
                    .debug_struct("WaitingChild")
                    .finish_non_exhaustive()
            }
        }

        impl portable_pty::ChildKiller for WaitingChild {
            fn kill(&mut self) -> io::Result<()> {
                self.kill_calls.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }

            fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
                Box::new(CountingKiller {
                    calls: self.kill_calls.clone(),
                })
            }
        }

        impl portable_pty::Child for WaitingChild {
            fn try_wait(&mut self) -> io::Result<Option<portable_pty::ExitStatus>> {
                match self.exited.lock().try_recv() {
                    Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        Ok(Some(portable_pty::ExitStatus::with_exit_code(0)))
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => Ok(None),
                }
            }

            fn wait(&mut self) -> io::Result<portable_pty::ExitStatus> {
                self.exited
                    .lock()
                    .recv()
                    .map_err(|err| io::Error::other(err.to_string()))?;
                Ok(portable_pty::ExitStatus::with_exit_code(0))
            }

            fn process_id(&self) -> Option<u32> {
                None
            }
        }

        fn target_with_invalid_os_ids(
            calls: Arc<std::sync::atomic::AtomicUsize>,
        ) -> TerminationTarget {
            TerminationTarget {
                killer: Box::new(CountingKiller { calls }),
                pid: Some(u32::MAX),
                process_group_leader_id: Some(i32::MAX),
                leader_stamp: None,
                retained: Vec::new(),
            }
        }

        fn test_pty_handles(
            master: Box<dyn portable_pty::MasterPty + Send>,
            writer: Box<dyn Write + Send>,
            size: Arc<AtomicU32>,
            log_reader: &LogReaderHandle,
        ) -> Result<(PtyHandles, PtyShutdown), Error> {
            let (events_tx, _events_rx) = mpsc::channel(1);
            PtyHandles::new(
                "svc".to_string(),
                RunId::new(1),
                events_tx,
                master,
                writer,
                size,
                log_reader,
            )
        }

        #[tokio::test]
        async fn failed_sigterm_uses_backend_without_skipping_escalation() {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut target = target_with_invalid_os_ids(calls.clone());
            let (events_tx, mut events_rx) = mpsc::channel(1);
            let service_id = "svc".to_string();
            let run_id = RunId::new(7);

            let started = target
                .request(
                    &events_tx,
                    &service_id,
                    run_id,
                    crate::spec::DEFAULT_STOP_GRACE_PERIOD,
                    crate::spec::StopSignal::default(),
                )
                .await;

            assert!(!started.escalated);
            assert!(started.kill_deadline.is_some());
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            target.force_kill();
            assert_eq!(calls.load(Ordering::Relaxed), 2);
            assert_matches!(
                events_rx.recv().await,
                Some(ProcessEvent::Killed {
                    service_id: killed_service_id,
                    run_id: killed_run_id,
                }) if killed_service_id == service_id && killed_run_id == run_id
            );
        }

        /// Regression: descendants that move into their own process group (as turborepo
        /// task runners do) must not survive the forced sweep, which previously reached
        /// only the leader's own group.
        #[tokio::test]
        async fn force_kill_reaps_descendants_that_left_the_process_group() -> eyre::Result<()> {
            use std::os::unix::process::CommandExt as _;

            // `set -m` enables job control, so the background sleep runs in its own
            // process group and killpg on the shell's group cannot reach it.
            let mut child = std::process::Command::new("bash")
                .args(["-c", "set -m; sleep 30 & wait"])
                .process_group(0)
                .spawn()?;
            let root = child.id();

            let root_stamp = crate::process_tree::stamp(root)
                .ok_or_else(|| eyre::eyre!("expected a stamp for the live shell"))?;

            // Wait for the escaped sleep to fork.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut escapees = crate::process_tree::descendants(root_stamp);
            while escapees.is_empty() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
                escapees = crate::process_tree::descendants(root_stamp);
            }
            if escapees.is_empty() {
                let _ = child.kill();
                let _ = child.wait();
                eyre::bail!("expected the escaped sleep to appear under the shell");
            }

            // The premise of the test is that `killpg` alone cannot reach these; if the
            // job were ever observed between `fork` and `setpgid` it would still be in
            // the leader's group and the test would silently degrade to a no-op.
            let leader_group = escapees
                .iter()
                .filter(|descendant| descendant.process_group != Some(root))
                .count();

            let mut target = TerminationTarget {
                killer: Box::new(CountingKiller {
                    calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                }),
                pid: Some(root),
                process_group_leader_id: Some(i32::try_from(root)?),
                leader_stamp: Some(root_stamp),
                retained: Vec::new(),
            };
            target.force_kill_swept().await;
            // Reap the shell so the leader cannot linger as a zombie during polling.
            // Kill first: if the sweep regressed, `wait` would otherwise block on
            // `sleep 30 & wait` and turn a clean failure into a near-timeout.
            let _ = child.kill();
            let _ = child.wait();

            // Poll for the escapees to disappear: the kernel still has to reparent the
            // orphans to init and reap them after the SIGKILL sweep.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let all_gone = loop {
                let all_gone = escapees
                    .iter()
                    .all(|descendant| !crate::process_tree::is_alive(descendant.stamp));
                if all_gone || std::time::Instant::now() >= deadline {
                    break all_gone;
                }
                std::thread::sleep(Duration::from_millis(25));
            };

            // Best-effort cleanup so a regression does not leak the sleeps.
            if !all_gone {
                for descendant in &escapees {
                    let _ = crate::process_tree::send_signal(descendant.stamp, Signal::SIGKILL);
                }
            }
            eyre::ensure!(
                leader_group > 0,
                "no descendant left the leader's group; killpg alone would have reaped these"
            );
            eyre::ensure!(
                all_gone,
                "escaped descendants survived force_kill: {escapees:?}"
            );
            Ok(())
        }

        /// The graceful stop request must deliver the configured signal: an INT trap
        /// writes a marker file, which SIGTERM, SIGHUP, SIGQUIT, or SIGKILL would all
        /// fail to run.
        #[tokio::test]
        async fn request_delivers_configured_stop_signal() -> eyre::Result<()> {
            use std::os::unix::process::CommandExt as _;

            let fixture_dir = crate::test_util::unique_tmp_dir("stop-signal");
            fs::create_dir_all(&fixture_dir)?;
            let marker = fixture_dir.join("trapped");
            let ready = fixture_dir.join("ready");
            // Announce readiness only once the trap is installed. Signalling before bash
            // reaches the `trap` builtin kills it on the default SIGINT disposition, so
            // without this handshake the test fails most of the time. Paths travel
            // through the environment because interpolating them into the script breaks
            // on any TMPDIR containing a space or quote.
            // The loop is bounded so a failing run cannot leave the fixture behind.
            let script = "trap 'echo done > \"$MICROMUX_MARKER\"; exit 0' INT; \
                          : > \"$MICROMUX_READY\"; for ((i=0;i<120;i++)); do sleep 1; done";
            let mut child = std::process::Command::new("bash")
                .args(["-c", script])
                .env("MICROMUX_MARKER", &marker)
                .env("MICROMUX_READY", &ready)
                .process_group(0)
                .spawn()?;
            let root = child.id();

            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !ready.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            let trap_installed = ready.exists();

            let mut target = TerminationTarget {
                killer: Box::new(CountingKiller {
                    calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                }),
                pid: Some(root),
                process_group_leader_id: Some(i32::try_from(root)?),
                leader_stamp: crate::process_tree::stamp(root),
                retained: Vec::new(),
            };
            let (events_tx, _events_rx) = mpsc::channel(1);
            target
                .request(
                    &events_tx,
                    &"svc".to_string(),
                    RunId::new(9),
                    Duration::from_secs(5),
                    crate::spec::StopSignal::Int,
                )
                .await;

            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !marker.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
            let marker_written = marker.exists();

            // Cleanup regardless of outcome so a failure does not leak the shell.
            let _ = nix::sys::signal::killpg(Pid::from_raw(i32::try_from(root)?), Signal::SIGKILL);
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_dir_all(&fixture_dir);

            eyre::ensure!(trap_installed, "fixture never installed its INT trap");
            eyre::ensure!(
                marker_written,
                "INT trap did not run; the configured stop signal was not delivered"
            );
            Ok(())
        }

        /// Regression: a descendant that ignores the stop signal must still be reaped
        /// when the leader itself exits promptly.
        ///
        /// The leader's exit ends the termination loop and reparents its descendants,
        /// so nothing can rediscover them afterwards. Only the snapshot retained from
        /// the graceful pass can still reach the escapee.
        #[tokio::test]
        async fn descendants_are_reaped_when_the_leader_exits_first() -> eyre::Result<()> {
            use std::os::unix::process::CommandExt as _;

            let fixture_dir = crate::test_util::unique_tmp_dir("leader-exits-first");
            fs::create_dir_all(&fixture_dir)?;
            let ready = fixture_dir.join("ready");
            // The background child ignores SIGTERM and runs in its own process group,
            // so neither the graceful signal nor a killpg can reap it. The leader exits
            // as soon as it is signalled. Everything is time-bounded: if the retention
            // this test guards ever regresses, the child is orphaned with no pid
            // recorded anywhere, and an unbounded fixture would leak it permanently.
            // The child blocks on one long sleep rather than respawning short ones,
            // which would race the sweep's final scan and flake the leak detection.
            let script = "set -m; (trap '' TERM; sleep 120 & : > \"$MICROMUX_READY\"; wait) & \
                 trap 'exit 0' TERM; for ((i=0;i<600;i++)); do sleep 0.2; done";
            let mut child = std::process::Command::new("bash")
                .args(["-c", script])
                .env("MICROMUX_READY", &ready)
                .process_group(0)
                .spawn()?;
            let root = child.id();
            let root_stamp = crate::process_tree::stamp(root)
                .ok_or_else(|| eyre::eyre!("expected a stamp for the live shell"))?;

            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !ready.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }

            let mut target = TerminationTarget {
                killer: Box::new(CountingKiller {
                    calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                }),
                pid: Some(root),
                process_group_leader_id: Some(i32::try_from(root)?),
                leader_stamp: Some(root_stamp),
                retained: Vec::new(),
            };
            let (events_tx, _events_rx) = mpsc::channel(1);
            target
                .request(
                    &events_tx,
                    &"svc".to_string(),
                    RunId::new(13),
                    Duration::from_millis(250),
                    crate::spec::StopSignal::Term,
                )
                .await;

            let escapees = std::mem::take(&mut target.retained);
            // The leader dies to its own TERM handler, mimicking the loop's wait branch.
            let _ = child.wait();

            let _ = reap_retained(
                &"svc".to_string(),
                RunId::new(13),
                escapees.clone(),
                None,
                Some(tokio::time::Instant::now() + Duration::from_millis(250)),
            )
            .await;

            // SIGKILL delivery is asynchronous; give the kernel a moment to reap.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let all_gone = loop {
                let all_gone = escapees
                    .iter()
                    .all(|descendant| !crate::process_tree::is_alive(descendant.stamp));
                if all_gone || std::time::Instant::now() >= deadline {
                    break all_gone;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            };

            // Best-effort cleanup so a regression does not leak the fixture.
            for descendant in &escapees {
                let _ = crate::process_tree::send_signal(descendant.stamp, Signal::SIGKILL);
            }
            let _ = fs::remove_dir_all(&fixture_dir);

            eyre::ensure!(
                !escapees.is_empty(),
                "expected the TERM-ignoring child to be retained from the graceful pass"
            );
            eyre::ensure!(
                all_gone,
                "descendant that ignored SIGTERM survived after the leader exited"
            );
            Ok(())
        }

        /// Dropping the spawn guard without disarming — the failed-startup path — must
        /// sweep the descendants the child managed to fork before setup failed,
        /// including ones that already escaped the process group: the child has been
        /// executing since spawn, so "it had no chance to fork" does not hold.
        #[tokio::test]
        async fn dropping_spawn_guard_sweeps_escaped_descendants() -> eyre::Result<()> {
            use std::os::unix::process::CommandExt as _;

            let fixture_dir = crate::test_util::unique_tmp_dir("guard-drop-sweep");
            fs::create_dir_all(&fixture_dir)?;
            let ready = fixture_dir.join("ready");
            let escapee_pid_file = fixture_dir.join("escapee-pid");
            // The escapee moves to its own group (`set -m`), out of the killpg's reach;
            // only the guard's walk can find it. Everything blocks on bounded sleeps.
            let script = "set -m; \
                 bash -c 'echo $$ > \"$MICROMUX_ESCAPEE_PID\"; sleep 120 & \
                          : > \"$MICROMUX_READY\"; wait' & \
                 wait";
            let mut child = std::process::Command::new("bash")
                .args(["-c", script])
                .env("MICROMUX_READY", &ready)
                .env("MICROMUX_ESCAPEE_PID", &escapee_pid_file)
                .process_group(0)
                .spawn()?;
            let root = child.id();
            let root_stamp = crate::process_tree::stamp(root)
                .ok_or_else(|| eyre::eyre!("expected a stamp for the live shell"))?;

            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !ready.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            let escapee_pid: i32 = fs::read_to_string(&escapee_pid_file)?.trim().parse()?;

            let guard = SpawnedChildGuard::new(
                Box::new(CountingKiller {
                    calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                }),
                Some(root),
                Some(i32::try_from(root)?),
                Some(root_stamp),
            );
            // No disarm: this is the startup-error path.
            drop(guard);

            // The kills are synchronous but the deaths are not; `ESRCH` proves the
            // escapee is fully retired.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let escapee_gone = loop {
                let gone = nix::sys::signal::kill(Pid::from_raw(escapee_pid), None)
                    == Err(nix::errno::Errno::ESRCH);
                if gone || std::time::Instant::now() >= deadline {
                    break gone;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            };

            // Best-effort cleanup so a regression does not leak the fixture.
            let _ = nix::sys::signal::kill(Pid::from_raw(escapee_pid), Signal::SIGKILL);
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_dir_all(&fixture_dir);

            eyre::ensure!(
                escapee_gone,
                "escapee {escapee_pid} survived the spawn guard's failed-startup sweep"
            );
            Ok(())
        }

        /// A descendant that outlived the service must be force-killed by the cleanup
        /// task rather than leaking silently — and so must the worker it forked after
        /// the retained snapshot, which only the pre-kill expansion can discover.
        ///
        /// The fixture ignores SIGTERM because the real target population has already
        /// survived the graceful pass; accepting any exit here would let a regression to
        /// SIGTERM pass unnoticed. Its single background sleep is forked before the
        /// readiness handshake, so the expansion observes it deterministically; the
        /// sleep itself bounds the fixture should a failing run leave it behind.
        #[tokio::test]
        async fn kill_verification_reaps_surviving_descendants() -> eyre::Result<()> {
            use std::os::unix::process::ExitStatusExt as _;

            let fixture_dir = crate::test_util::unique_tmp_dir("kill-verification");
            fs::create_dir_all(&fixture_dir)?;
            let ready = fixture_dir.join("ready");
            let worker_pid_file = fixture_dir.join("worker-pid");
            let mut child = std::process::Command::new("bash")
                .args([
                    "-c",
                    "trap '' TERM; sleep 120 & echo $! > \"$MICROMUX_WORKER_PID\"; \
                     : > \"$MICROMUX_READY\"; wait",
                ])
                .env("MICROMUX_READY", &ready)
                .env("MICROMUX_WORKER_PID", &worker_pid_file)
                .spawn()?;
            let stamp = crate::process_tree::stamp(child.id())
                .ok_or_else(|| eyre::eyre!("expected a stamp for the live fixture"))?;

            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !ready.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            let worker_pid: u32 = fs::read_to_string(&worker_pid_file)?.trim().parse()?;

            // Hand the live process to the reaper as if the sweep had missed it;
            // `kill_at: None` mirrors the escalation path where the grace window has
            // already elapsed, so the SIGKILL is delivered before this returns. The
            // worker is deliberately absent from the retained set: only the expansion
            // can reach it.
            let survivors = reap_retained(
                &"svc".to_string(),
                RunId::new(11),
                vec![crate::process_tree::Descendant {
                    stamp,
                    process_group: None,
                }],
                None,
                None,
            )
            .await;
            let worker_swept = survivors.iter().any(|survivor| survivor.pid == worker_pid);

            // The child is ours, so its death is observable via try_wait. The deadline
            // tracks the verification interval so raising that constant does not
            // silently break this test.
            let deadline = std::time::Instant::now() + KILL_VERIFICATION_INTERVAL * 25;
            let mut status = None;
            while status.is_none() && std::time::Instant::now() < deadline {
                status = child.try_wait()?;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            // The verification tail must observe the deaths and return without incident.
            verify_reaped(&"svc".to_string(), RunId::new(11), survivors).await;

            // The worker reparented on the fixture's death; wait for the kernel to
            // fully retire it. `kill -0` is used rather than a stamp read: it keeps
            // succeeding for an unreaped zombie and cannot transiently misreport a
            // live process under load, so `ESRCH` proves the worker — and its copies
            // of the test's output handles — are certainly gone before the test exits.
            let worker_gone = match i32::try_from(worker_pid) {
                Ok(pid) => {
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    loop {
                        let gone = nix::sys::signal::kill(Pid::from_raw(pid), None)
                            == Err(nix::errno::Errno::ESRCH);
                        if gone || std::time::Instant::now() >= deadline {
                            break gone;
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }
                Err(_) => false,
            };

            // Best-effort cleanup so a regression does not leak the fixture.
            let _ = child.kill();
            let _ = child.wait();
            if let Ok(worker_pid) = i32::try_from(worker_pid) {
                let _ = nix::sys::signal::kill(Pid::from_raw(worker_pid), Signal::SIGKILL);
            }
            let _ = fs::remove_dir_all(&fixture_dir);

            let status = status
                .ok_or_else(|| eyre::eyre!("the reaper did not kill the surviving descendant"))?;
            eyre::ensure!(
                status.signal() == Some(nix::libc::SIGKILL),
                "the reaper must escalate to SIGKILL, got {status:?}"
            );
            eyre::ensure!(
                worker_swept,
                "the expansion must sweep the worker the retained stamps did not name"
            );
            eyre::ensure!(
                worker_gone,
                "the swept worker survived the forced termination"
            );
            Ok(())
        }

        #[tokio::test]
        async fn saturated_event_channel_does_not_delay_or_drop_termination_signal() {
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut target = target_with_invalid_os_ids(calls.clone());
            let (events_tx, mut events_rx) = mpsc::channel(1);
            events_tx
                .send(ProcessEvent::Killed {
                    service_id: "occupied".to_string(),
                    run_id: RunId::new(1),
                })
                .await
                .expect("test channel should accept its first event");

            let mut started = target
                .request(
                    &events_tx,
                    &"svc".to_string(),
                    RunId::new(7),
                    Duration::from_secs(1),
                    crate::spec::StopSignal::default(),
                )
                .await;

            assert!(started.kill_deadline.is_some());
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert!(started.pending_notification.is_some());
            assert_matches!(
                events_rx.recv().await,
                Some(ProcessEvent::Killed { service_id, .. }) if service_id == "occupied"
            );
            if let Some(notification) = started.pending_notification.take() {
                assert!(notification.await.is_ok());
            }
            assert_matches!(
                events_rx.recv().await,
                Some(ProcessEvent::Killed {
                    service_id,
                    run_id,
                }) if service_id == "svc" && run_id == RunId::new(7)
            );
        }

        #[test]
        fn bounded_log_split_preserves_utf8_characters() {
            let line = format!("x{}tail", "é".repeat(PTY_LOG_LINE_MAX_BYTES / 2 + 1));
            let split = bounded_line_split(line.as_bytes());

            assert!(std::str::from_utf8(&line.as_bytes()[..split]).is_ok());
            assert!(std::str::from_utf8(&line.as_bytes()[split..]).is_ok());
            assert!(split <= PTY_LOG_LINE_MAX_BYTES);
            assert_eq!(
                bounded_line_split(&vec![0x80; PTY_LOG_LINE_MAX_BYTES + 1]),
                PTY_LOG_LINE_MAX_BYTES
            );
        }

        #[test]
        fn paste_occupies_one_atomic_writer_queue_entry() -> eyre::Result<()> {
            let (write_queue, queued) = std_mpsc::sync_channel(1);
            let handles = PtyHandles {
                master: Arc::new(Mutex::new(None)),
                write_queue: Arc::new(Mutex::new(Some(write_queue))),
                size: Arc::new(AtomicU32::new(0)),
            };
            let paste = vec![b'x'; MAX_PTY_INPUT_BATCH_BYTES + 17];
            let budget = InputBudget::shared_with_limit(MAX_PTY_PASTE_BYTES);
            let input = PreparedPtyInput::prepare(
                &budget,
                "svc".to_string(),
                paste.clone(),
                PtyInputKind::Paste,
            )?;

            handles.write_input(input)?;

            match queued.try_recv() {
                Ok(PtyWrite::Input(input)) if input.kind() == PtyInputKind::Paste => {
                    assert_eq!(input.data(), paste);
                }
                Ok(PtyWrite::Input(_) | PtyWrite::TerminalResponse(_)) => {
                    panic!("paste was split into another queue entry")
                }
                Err(err) => panic!("paste was not queued: {err}"),
            }
            assert_matches!(queued.try_recv(), Err(std_mpsc::TryRecvError::Empty));
            Ok(())
        }

        #[test]
        fn input_byte_budget_is_shared_across_service_queues() -> eyre::Result<()> {
            let input_budget = InputBudget::shared_with_limit(2 * MAX_PTY_PASTE_BYTES);
            let (first_queue, first_queued) = std_mpsc::sync_channel(2);
            let first = PtyHandles {
                master: Arc::new(Mutex::new(None)),
                write_queue: Arc::new(Mutex::new(Some(first_queue))),
                size: Arc::new(AtomicU32::new(0)),
            };
            let (second_queue, second_queued) = std_mpsc::sync_channel(2);
            let second = PtyHandles {
                master: Arc::new(Mutex::new(None)),
                write_queue: Arc::new(Mutex::new(Some(second_queue))),
                size: Arc::new(AtomicU32::new(0)),
            };
            let paste = vec![b'x'; MAX_PTY_PASTE_BYTES];

            first.write_input(PreparedPtyInput::prepare(
                &input_budget,
                "first".to_string(),
                paste.clone(),
                PtyInputKind::Paste,
            )?)?;
            second.write_input(PreparedPtyInput::prepare(
                &input_budget,
                "second".to_string(),
                paste.clone(),
                PtyInputKind::Paste,
            )?)?;
            assert_matches!(
                PreparedPtyInput::prepare(
                    &input_budget,
                    "first".to_string(),
                    paste.clone(),
                    PtyInputKind::Paste,
                ),
                Err(PtyInputPrepareError::BudgetExhausted { .. })
            );

            drop(second_queued.recv()?);
            first.write_input(PreparedPtyInput::prepare(
                &input_budget,
                "first".to_string(),
                paste,
                PtyInputKind::Paste,
            )?)?;
            drop(first_queued);
            Ok(())
        }

        #[cfg(target_vendor = "apple")]
        #[test]
        fn apple_poller_reuses_one_kqueue_across_waits() -> eyre::Result<()> {
            let poller = PtyPoller::new()?;
            let queue_fd = poller.raw_fd();
            let mut pipe = Pipe::new()?;
            pipe.write.write_all(b"x")?;
            let mut descriptors = [pollfd {
                fd: pipe.read.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            }];

            for _ in 0..2 {
                let descriptor = descriptors
                    .first_mut()
                    .ok_or_eyre("test descriptor is missing")?;
                descriptor.revents = 0;
                assert_eq!(
                    poller.poll(&mut descriptors, Some(Duration::from_secs(1)))?,
                    1
                );
                let descriptor = descriptors
                    .first()
                    .ok_or_eyre("test descriptor is missing")?;
                assert_ne!(descriptor.revents & POLLIN, 0);
                assert_eq!(poller.raw_fd(), queue_fd);
            }
            Ok(())
        }

        #[test]
        fn pty_input_write_times_out_when_the_slave_queue_is_full() -> eyre::Result<()> {
            let fixture_dir = tempfile::tempdir()?;
            let ready_file = fixture_dir.path().join("ready");
            let pair = portable_pty::native_pty_system()
                .openpty(portable_pty::PtySize::default())
                .map_err(|err| eyre::eyre!("failed to open test PTY: {err}"))?;
            let mut command = portable_pty::CommandBuilder::new("sh");
            command.args(["-c", "stty raw -echo; : > \"$READY\"; exec sleep 60"]);
            command.env("READY", &ready_file);
            let mut child = pair
                .slave
                .spawn_command(command)
                .map_err(|err| eyre::eyre!("failed to spawn stalled raw-mode reader: {err}"))?;
            let poll_write = configure_nonblocking_pty(pair.master.as_ref())?;
            let (cancel_read, cancellation) = PtyCancellation::pipe()?;
            let writer = pair
                .master
                .take_writer()
                .map_err(|err| eyre::eyre!("failed to take test PTY writer: {err}"))?;
            let (events_tx, _events_rx) = mpsc::channel(1);
            let mut writer = PtyWriter {
                service_id: "svc".to_string(),
                run_id: RunId::new(1),
                events_tx,
                writer,
                cancellation,
                poll_write,
                cancel_read,
                poller: PtyPoller::new()?,
            };

            let ready_deadline = Instant::now() + Duration::from_secs(3);
            while !ready_file.exists() {
                if Instant::now() >= ready_deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    eyre::bail!("stalled raw-mode reader did not become ready");
                }
                thread::sleep(Duration::from_millis(10));
            }

            // The slave never drains its raw-mode input queue, so the worker must abandon this
            // batch at its deadline instead of remaining occupied forever.
            let started_at = Instant::now();
            let write_result = writer.write_input(&vec![b'x'; 1024 * 1024]);
            let write_elapsed = started_at.elapsed();

            let _ = child.kill();
            let _ = child.wait();
            if !matches!(
                &write_result,
                Err(err) if err.kind() == io::ErrorKind::TimedOut
            ) {
                eyre::bail!("stalled PTY write did not time out: {write_result:?}");
            }
            let earliest_timeout =
                PTY_INPUT_WRITE_TIMEOUT.saturating_sub(Duration::from_millis(100));
            if write_elapsed < earliest_timeout {
                eyre::bail!("stalled PTY write returned before the retry deadline");
            }
            if write_elapsed >= Duration::from_secs(3) {
                eyre::bail!("stalled PTY write exceeded its retry deadline");
            }
            Ok(())
        }

        #[test]
        fn active_pty_input_retains_its_global_budget_without_blocking_enqueue() -> eyre::Result<()>
        {
            let fixture_dir = tempfile::tempdir()?;
            let ready_file = fixture_dir.path().join("ready");
            let pair = portable_pty::native_pty_system()
                .openpty(portable_pty::PtySize::default())
                .map_err(|err| eyre::eyre!("failed to open test PTY: {err}"))?;
            let mut command = portable_pty::CommandBuilder::new("sh");
            command.args(["-c", "stty raw -echo; : > \"$READY\"; exec sleep 60"]);
            command.env("READY", &ready_file);
            let mut child = pair
                .slave
                .spawn_command(command)
                .map_err(|err| eyre::eyre!("failed to spawn stalled raw-mode reader: {err}"))?;
            let writer = pair
                .master
                .take_writer()
                .map_err(|err| eyre::eyre!("failed to take test PTY writer: {err}"))?;
            let (would_block_tx, would_block_rx) = std::sync::mpsc::channel();
            let writer = ExitOnDropWriter {
                writer: Some(writer),
                exited: None,
                would_block: Some(would_block_tx),
            };
            let log_reader = LogReaderHandle::test_dummy();
            let (events_tx, mut events_rx) = mpsc::channel(1);
            let (handles, shutdown) = PtyHandles::new(
                "svc".to_string(),
                RunId::new(7),
                events_tx,
                pair.master,
                Box::new(writer),
                Arc::new(AtomicU32::new(0)),
                &log_reader,
            )?;

            let ready_deadline = Instant::now() + Duration::from_secs(3);
            while !ready_file.exists() {
                if Instant::now() >= ready_deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    eyre::bail!("stalled raw-mode reader did not become ready");
                }
                thread::sleep(Duration::from_millis(10));
            }

            // Enqueuing is the scheduler-facing operation. It must return well before the writer
            // worker reaches its backpressure deadline.
            let started_at = Instant::now();
            let budget = InputBudget::shared_with_limit(MAX_PTY_INPUT_BATCH_BYTES);
            handles.write_input(PreparedPtyInput::prepare(
                &budget,
                "svc".to_string(),
                vec![b'x'; MAX_PTY_INPUT_BATCH_BYTES],
                PtyInputKind::Input,
            )?)?;
            let enqueue_elapsed = started_at.elapsed();
            would_block_rx
                .recv_timeout(Duration::from_secs(3))
                .map_err(|err| eyre::eyre!("PTY writer did not reach backpressure: {err}"))?;
            assert_matches!(
                PreparedPtyInput::prepare(
                    &budget,
                    "svc".to_string(),
                    vec![b'y'],
                    PtyInputKind::Input,
                ),
                Err(PtyInputPrepareError::BudgetExhausted { .. })
            );
            let event_deadline = Instant::now() + Duration::from_secs(3);
            let dropped = loop {
                match events_rx.try_recv() {
                    Ok(event) => break event,
                    Err(mpsc::error::TryRecvError::Empty) if Instant::now() < event_deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => eyre::bail!("PTY writer did not report discarded input: {err}"),
                }
            };
            assert_matches!(
                dropped,
                ProcessEvent::InputDropped {
                    service_id,
                    run_id,
                    input_kind: "input",
                    ..
                } if service_id == "svc" && run_id == RunId::new(7)
            );

            shutdown.close();
            let _ = child.kill();
            let _ = child.wait();
            let latest_enqueue = PTY_INPUT_WRITE_TIMEOUT.saturating_sub(Duration::from_millis(100));
            if enqueue_elapsed >= latest_enqueue {
                eyre::bail!("PTY input enqueue waited for slave backpressure");
            }
            Ok(())
        }

        #[test]
        fn pty_input_write_retries_backpressure_for_an_active_raw_reader() -> eyre::Result<()> {
            const INPUT_LEN: usize = 64 * 1024;

            let expected_len = u64::try_from(INPUT_LEN)?;
            let fixture_dir = tempfile::tempdir()?;
            let ready_file = fixture_dir.path().join("ready");
            let output_file = fixture_dir.path().join("output");
            let pair = portable_pty::native_pty_system()
                .openpty(portable_pty::PtySize::default())
                .map_err(|err| eyre::eyre!("failed to open test PTY: {err}"))?;
            let mut command = portable_pty::CommandBuilder::new("sh");
            command.args([
                "-c",
                "stty raw -echo; : > \"$READY\"; exec head -c \"$INPUT_LEN\" > \"$OUTPUT\"",
            ]);
            command.env("READY", &ready_file);
            command.env("OUTPUT", &output_file);
            command.env("INPUT_LEN", INPUT_LEN.to_string());
            let mut child = pair
                .slave
                .spawn_command(command)
                .map_err(|err| eyre::eyre!("failed to spawn raw-mode reader: {err}"))?;
            let writer = pair
                .master
                .take_writer()
                .map_err(|err| eyre::eyre!("failed to take test PTY writer: {err}"))?;
            let log_reader = LogReaderHandle::test_dummy();
            let (handles, shutdown) = test_pty_handles(
                pair.master,
                writer,
                Arc::new(AtomicU32::new(0)),
                &log_reader,
            )?;

            let transfer_result = (|| -> eyre::Result<()> {
                let ready_deadline = Instant::now() + Duration::from_secs(3);
                while !ready_file.exists() {
                    if Instant::now() >= ready_deadline {
                        eyre::bail!("raw-mode reader did not become ready");
                    }
                    thread::sleep(Duration::from_millis(10));
                }

                let input = vec![b'x'; INPUT_LEN];
                let budget = InputBudget::shared_with_limit(INPUT_LEN);
                handles.write_input(PreparedPtyInput::prepare(
                    &budget,
                    "svc".to_string(),
                    input,
                    PtyInputKind::Input,
                )?)?;

                // Raw-mode queues are much smaller than this paste.
                //
                // The complete output proves transient `EAGAIN` was retried rather than treated as
                // a terminal write failure.
                let output_deadline = Instant::now() + Duration::from_secs(3);
                while fs::metadata(&output_file).map_or(0, |metadata| metadata.len()) < expected_len
                {
                    if Instant::now() >= output_deadline {
                        eyre::bail!("raw-mode reader received truncated input");
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(())
            })();

            if transfer_result.is_err() {
                let _ = child.kill();
            }
            let wait_result = child.wait();
            shutdown.close();
            transfer_result?;
            let status = wait_result?;
            if !status.success() {
                eyre::bail!("raw-mode reader exited with {status}");
            }
            Ok(())
        }

        #[test]
        fn pty_input_timeout_measures_stalls_instead_of_total_transfer_time() -> eyre::Result<()> {
            const INPUT_LEN: usize = 128 * 1024;
            const READ_SIZE: usize = 8 * 1024;
            const READ_INTERVAL: Duration = Duration::from_millis(100);

            let mut pipe = Pipe::new()?;
            pipe.write.set_non_blocking(true)?;
            let poll_write = pipe.write.try_clone()?;
            let (cancel_read, cancellation) = PtyCancellation::pipe()?;
            let (events_tx, _events_rx) = mpsc::channel(1);
            let mut writer = PtyWriter {
                service_id: "svc".to_string(),
                run_id: RunId::new(1),
                events_tx,
                writer: Box::new(pipe.write),
                cancellation,
                poll_write,
                cancel_read,
                poller: PtyPoller::new()?,
            };
            let reader = thread::spawn(move || -> io::Result<usize> {
                let mut reader = pipe.read;
                let mut total = 0;
                let mut buffer = [0; READ_SIZE];
                loop {
                    let read = reader.read(&mut buffer)?;
                    if read == 0 {
                        return Ok(total);
                    }
                    total += read;
                    thread::sleep(READ_INTERVAL);
                }
            });

            let started = Instant::now();
            let result = writer.write_input(&vec![b'x'; INPUT_LEN]);
            let elapsed = started.elapsed();
            drop(writer);
            let received = reader
                .join()
                .map_err(|_| eyre::eyre!("slow pipe reader panicked"))??;

            result?;
            assert_eq!(received, INPUT_LEN);
            if elapsed <= PTY_INPUT_WRITE_TIMEOUT {
                eyre::bail!("test transfer did not outlive one stall interval");
            }
            Ok(())
        }

        #[test]
        fn input_drop_notification_cannot_pin_the_pty_writer() -> eyre::Result<()> {
            struct FailingWriter;

            impl Write for FailingWriter {
                fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "injected PTY write failure",
                    ))
                }

                fn flush(&mut self) -> io::Result<()> {
                    Ok(())
                }
            }

            let (events_tx, mut events_rx) = mpsc::channel(1);
            events_tx.try_send(ProcessEvent::Killed {
                service_id: "occupied".to_string(),
                run_id: RunId::new(1),
            })?;
            let poll_pipe = Pipe::new()?;
            let (cancel_read, cancellation) = PtyCancellation::pipe()?;
            let writer = PtyWriter {
                service_id: "svc".to_string(),
                run_id: RunId::new(7),
                events_tx,
                writer: Box::new(FailingWriter),
                cancellation,
                poll_write: poll_pipe.write,
                cancel_read,
                poller: PtyPoller::new()?,
            };
            let (write_tx, write_rx) = std_mpsc::sync_channel(1);
            let budget = InputBudget::shared_with_limit(1);
            let input = PreparedPtyInput::prepare(
                &budget,
                "svc".to_string(),
                vec![b'x'],
                PtyInputKind::Input,
            )?;
            write_tx.send(PtyWrite::Input(input))?;
            drop(write_tx);
            let (finished_tx, finished_rx) = std_mpsc::channel();
            let worker = thread::spawn(move || {
                writer.run(&write_rx);
                let _ = finished_tx.send(());
            });

            if let Err(err) = finished_rx.recv_timeout(Duration::from_secs(1)) {
                // Release an implementation that accidentally blocks on the full event channel so
                // the test does not leak its worker while reporting the regression.
                let _ = events_rx.try_recv();
                let _ = worker.join();
                return Err(eyre::eyre!(
                    "PTY writer was pinned by its notification: {err}"
                ));
            }
            worker
                .join()
                .map_err(|_| eyre::eyre!("PTY writer thread panicked"))?;

            assert_matches!(
                events_rx.try_recv(),
                Ok(ProcessEvent::Killed { service_id, .. }) if service_id == "occupied"
            );
            PreparedPtyInput::prepare(&budget, "svc".to_string(), vec![b'y'], PtyInputKind::Input)?;
            Ok(())
        }

        #[test]
        fn pty_terminal_response_survives_extended_backpressure() -> eyre::Result<()> {
            const RESPONSE_LEN: usize = 64 * 1024;

            let expected_len = u64::try_from(RESPONSE_LEN)?;
            let fixture_dir = tempfile::tempdir()?;
            let ready_file = fixture_dir.path().join("ready");
            let output_file = fixture_dir.path().join("output");
            let pair = portable_pty::native_pty_system()
                .openpty(portable_pty::PtySize::default())
                .map_err(|err| eyre::eyre!("failed to open test PTY: {err}"))?;
            let mut command = portable_pty::CommandBuilder::new("sh");
            command.args([
                "-c",
                "stty raw -echo; : > \"$READY\"; sleep 1; \
                 exec head -c \"$RESPONSE_LEN\" > \"$OUTPUT\"",
            ]);
            command.env("READY", &ready_file);
            command.env("OUTPUT", &output_file);
            command.env("RESPONSE_LEN", RESPONSE_LEN.to_string());
            let mut child = pair
                .slave
                .spawn_command(command)
                .map_err(|err| eyre::eyre!("failed to spawn delayed raw-mode reader: {err}"))?;
            let writer = pair
                .master
                .take_writer()
                .map_err(|err| eyre::eyre!("failed to take test PTY writer: {err}"))?;
            let log_reader = LogReaderHandle::test_dummy();
            let (handles, shutdown) = test_pty_handles(
                pair.master,
                writer,
                Arc::new(AtomicU32::new(0)),
                &log_reader,
            )?;

            let transfer_result = (|| -> eyre::Result<()> {
                let ready_deadline = Instant::now() + Duration::from_secs(3);
                while !ready_file.exists() {
                    if Instant::now() >= ready_deadline {
                        eyre::bail!("delayed raw-mode reader did not become ready");
                    }
                    thread::sleep(Duration::from_millis(10));
                }

                let response = vec![b'x'; RESPONSE_LEN];
                handles
                    .write_queue
                    .lock()
                    .as_ref()
                    .ok_or_eyre("PTY writer closed before terminal response")?
                    .try_send(PtyWrite::TerminalResponse(response))
                    .map_err(|err| eyre::eyre!("failed to queue terminal response: {err}"))?;

                // The slave waits longer than the input-write deadline before reading.
                //
                // A complete output proves terminal replies retain their unwritten tail across
                // backpressure.
                let output_deadline = Instant::now() + Duration::from_secs(4);
                while fs::metadata(&output_file).map_or(0, |metadata| metadata.len()) < expected_len
                {
                    if Instant::now() >= output_deadline {
                        eyre::bail!("raw-mode reader received a truncated terminal response");
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(())
            })();

            if transfer_result.is_err() {
                let _ = child.kill();
            }
            let wait_result = child.wait();
            shutdown.close();
            transfer_result?;
            let status = wait_result?;
            if !status.success() {
                eyre::bail!("delayed raw-mode reader exited with {status}");
            }
            Ok(())
        }

        #[test]
        fn pty_shutdown_closes_interactive_handles_and_wakes_reader() -> eyre::Result<()> {
            let pair = portable_pty::native_pty_system()
                .openpty(portable_pty::PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|err| eyre::eyre!("failed to open test PTY: {err}"))?;
            let (mut reader, log_reader) = PtyOutputReader::new(pair.master.as_ref())?;
            let writer = pair
                .master
                .take_writer()
                .map_err(|err| eyre::eyre!("failed to take test PTY writer: {err}"))?;
            let size = Arc::new(AtomicU32::new(0));
            let (handles, shutdown) = test_pty_handles(pair.master, writer, size, &log_reader)?;

            shutdown.close();

            // Closing the interactive handles alone is insufficient because the reader owns two
            // duplicated master descriptors.
            //
            // Its cancellation signal must also become readable.
            assert!(handles.master.lock().is_none());
            assert!(handles.write_queue.lock().is_none());
            assert_eq!(reader.read(&mut [0; 1])?, None);
            Ok(())
        }

        #[test]
        fn dropping_pty_shutdown_cancels_a_blocked_terminal_response() -> eyre::Result<()> {
            const RESPONSE_LEN: usize = 1024 * 1024;

            let fixture_dir = tempfile::tempdir()?;
            let ready_file = fixture_dir.path().join("ready");
            let pair = portable_pty::native_pty_system()
                .openpty(portable_pty::PtySize::default())
                .map_err(|err| eyre::eyre!("failed to open test PTY: {err}"))?;
            let mut command = portable_pty::CommandBuilder::new("sh");
            command.args(["-c", "stty raw -echo; : > \"$READY\"; exec sleep 60"]);
            command.env("READY", &ready_file);
            let mut child = pair
                .slave
                .spawn_command(command)
                .map_err(|err| eyre::eyre!("failed to spawn stalled raw-mode reader: {err}"))?;
            let writer = pair
                .master
                .take_writer()
                .map_err(|err| eyre::eyre!("failed to take test PTY writer: {err}"))?;
            let (would_block_tx, would_block_rx) = std::sync::mpsc::channel();
            let (writer_dropped_tx, writer_dropped_rx) = std::sync::mpsc::channel();
            let writer = ExitOnDropWriter {
                writer: Some(writer),
                exited: Some(writer_dropped_tx),
                would_block: Some(would_block_tx),
            };
            let log_reader = LogReaderHandle::test_dummy();
            let (handles, shutdown) = test_pty_handles(
                pair.master,
                Box::new(writer),
                Arc::new(AtomicU32::new(0)),
                &log_reader,
            )?;

            let test_result = (|| -> eyre::Result<()> {
                let ready_deadline = Instant::now() + Duration::from_secs(3);
                while !ready_file.exists() {
                    if Instant::now() >= ready_deadline {
                        eyre::bail!("stalled raw-mode reader did not become ready");
                    }
                    thread::sleep(Duration::from_millis(10));
                }

                handles
                    .write_queue
                    .lock()
                    .as_ref()
                    .ok_or_eyre("PTY writer closed before terminal response")?
                    .try_send(PtyWrite::TerminalResponse(vec![b'x'; RESPONSE_LEN]))
                    .map_err(|err| eyre::eyre!("failed to queue terminal response: {err}"))?;
                would_block_rx
                    .recv_timeout(Duration::from_secs(3))
                    .map_err(|err| eyre::eyre!("PTY writer did not reach backpressure: {err}"))?;

                // Keep the queue and slave open so only the shutdown capability's drop can wake
                // the blocked worker and release its writer endpoint.
                drop(shutdown);
                writer_dropped_rx
                    .recv_timeout(Duration::from_secs(1))
                    .map_err(|err| {
                        eyre::eyre!("PTY writer remained blocked after teardown: {err}")
                    })?;
                assert!(handles.write_queue.lock().is_some());
                assert!(handles.master.lock().is_some());
                Ok(())
            })();

            let _ = child.kill();
            let _ = child.wait();
            test_result
        }

        #[tokio::test]
        async fn escalation_closes_the_pty_before_reporting_exit() -> eyre::Result<()> {
            let pair = portable_pty::native_pty_system()
                .openpty(portable_pty::PtySize::default())
                .map_err(|err| eyre::eyre!("failed to open test PTY: {err}"))?;
            let writer = pair
                .master
                .take_writer()
                .map_err(|err| eyre::eyre!("failed to take test PTY writer: {err}"))?;
            let (child_exited_tx, child_exited_rx) = std::sync::mpsc::channel();
            let writer = ExitOnDropWriter {
                writer: Some(writer),
                exited: Some(child_exited_tx),
                would_block: None,
            };
            let log_reader = LogReaderHandle::test_dummy();
            let (handles, pty_shutdown) = test_pty_handles(
                pair.master,
                Box::new(writer),
                Arc::new(AtomicU32::new(0)),
                &log_reader,
            )?;
            let _slave = pair.slave;

            let kill_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let child = WaitingChild {
                exited: Mutex::new(child_exited_rx),
                kill_calls: kill_calls.clone(),
            };
            let (events_tx, mut events_rx) = mpsc::channel(4);
            let shutdown = CancellationToken::new();
            let terminate = CancellationToken::new();
            spawn_termination_task_with_timing(
                TerminationTaskArgs {
                    service_id: "svc".to_string(),
                    run_id: RunId::new(7),
                    events_tx,
                    shutdown,
                    terminate: terminate.clone(),
                    killer: Box::new(CountingKiller {
                        calls: kill_calls.clone(),
                    }),
                    pid: None,
                    process_group_leader_id: None,
                    stop_signal: crate::spec::StopSignal::default(),
                    leader_stamp: None,
                    pty_shutdown,
                    child: Box::new(child),
                    health_task: None,
                },
                TerminationTiming {
                    force_kill_after: Duration::from_millis(10),
                    pty_hangup_after: Duration::from_millis(10),
                },
            );

            terminate.cancel();
            let killed = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
                .await?
                .ok_or_eyre("termination task ended without a killed event")?;
            assert_matches!(killed, ProcessEvent::Killed { .. });

            // The fake child ignores both kill requests and finishes only when PTY shutdown drops
            // its writer.
            //
            // Receiving `Exited` therefore exercises the escalation deadline and close path.
            let exited = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
                .await?
                .ok_or_eyre("termination task ended without an exit event")?;
            assert_matches!(exited, ProcessEvent::Exited { exit_code: 0, .. });
            assert_eq!(kill_calls.load(Ordering::Relaxed), 2);
            assert!(handles.write_queue.lock().is_none());
            assert!(handles.master.lock().is_none());
            Ok(())
        }
    }

    #[test]
    fn rate_limit_increments_id_on_alt_screen_transition() {
        let mut rate = RateLimit::new();

        assert_eq!(rate.snapshot_id(), 0);
        rate.set_alt_screen(true);
        assert_eq!(rate.snapshot_id(), 1);
        rate.set_alt_screen(false);
        assert_eq!(rate.snapshot_id(), 2);
    }

    #[test]
    fn rate_limit_window_reset_keeps_snapshot_id() {
        let mut rate = RateLimit::new();
        let start = Instant::now();
        rate.set_alt_screen(true);
        rate.window_start = start;
        let snapshot_id = rate.snapshot_id();

        for _ in 0..ALT_SCREEN_MAX_UPDATES_PER_SEC {
            assert_eq!(rate.snapshot_decision(false, start), SnapshotDecision::Emit);
            rate.record_snapshot_sent();
        }

        assert_eq!(
            rate.snapshot_decision(false, start + Duration::from_secs(1)),
            SnapshotDecision::Emit
        );
        assert_eq!(rate.snapshot_id(), snapshot_id);
    }

    #[test]
    fn rate_limit_warning_keeps_snapshot_id() {
        let mut rate = RateLimit::new();
        let start = Instant::now();
        rate.set_alt_screen(true);
        rate.window_start = start;
        let snapshot_id = rate.snapshot_id();

        for _ in 0..ALT_SCREEN_MAX_UPDATES_PER_SEC {
            assert_eq!(rate.snapshot_decision(false, start), SnapshotDecision::Emit);
            rate.record_snapshot_sent();
        }

        assert_eq!(
            rate.snapshot_decision(false, start + Duration::from_millis(100)),
            SnapshotDecision::Warn
        );
        assert_eq!(rate.snapshot_id(), snapshot_id);
        assert_eq!(
            rate.snapshot_decision(false, start + Duration::from_millis(100)),
            SnapshotDecision::Drop
        );
        assert_eq!(rate.snapshot_id(), snapshot_id);
    }
}
