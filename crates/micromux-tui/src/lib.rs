//! `micromux-tui` provides the terminal user interface for micromux.
//!
//! The main entry point is [`App`], constructed from a [`SessionSource`], an optional PTY-input
//! sender, a shutdown token, and display options; call [`App::render`] to run it. The TUI holds only
//! view state and re-reads its synchronous source on each [`micromux::SessionChange`].

mod all_logs;
mod cells;
mod clipboard;
mod event;
mod json_log;
mod level;
mod log_text;
mod mouse;
mod platform;
mod remote;
mod render;
mod search;
mod source;
mod state;
mod style;
mod terminal;
mod timeline;

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use micromux::{
    ChangeKind, PreparedPtyInput, PtyInputPrepareError, PtyInputSendError, SessionChange,
    TerminalControl,
};
use ratatui::DefaultTerminal;
use tokio::sync::broadcast;

pub use remote::RemoteSource;
pub use source::{LocalSource, SessionSource};

const FRAME_INTERVAL: Duration = Duration::from_millis(16);
const PENDING_PTY_INPUT_RETRY: Duration = Duration::from_millis(5);
/// How long the footer says how to quit after a Ctrl-C.
const QUIT_HINT_DURATION: Duration = Duration::from_secs(2);
const TERMINAL_INPUT_CLOSED_NOTICE: &str = "terminal input closed; send SIGINT or SIGTERM to exit";
const DISCONNECTED_INPUT_NOTICE: &str = "input discarded while reconnecting";

fn format_byte_limit(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = 1024 * KIB;
    if bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else if bytes.is_multiple_of(KIB) {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

/// Initial presentation of the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewOptions {
    /// Render structured JSON logs as compact colored lines instead of raw JSON.
    pub pretty_json: bool,
    /// How the TUI handles the mouse.
    pub mouse: MouseOptions,
}

impl Default for ViewOptions {
    fn default() -> Self {
        Self {
            pretty_json: true,
            mouse: MouseOptions::default(),
        }
    }
}

/// How the TUI handles the mouse.
///
/// The TUI captures the mouse when any of `select`, `navigate` and `scroll` is on.
/// Without capture the terminal keeps its own mouse handling, such as selecting text across the
/// whole screen; the `m` key switches capture at runtime either way, back to the parts that are
/// on, or to all of them when none is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseOptions {
    /// A drag, a double or a triple click selects text inside one pane, and a right click
    /// copies it, or the URL it lands on.
    pub select: bool,
    /// A click focuses panes, selects services, presses key hints, opens URLs and picks levels.
    pub navigate: bool,
    /// The wheel scrolls the pane, list or popup under the pointer.
    pub scroll: bool,
    /// Copy a selection to the clipboard as soon as the drag ends, rather than only on an
    /// explicit copy key.
    pub copy_on_select: bool,
}

impl Default for MouseOptions {
    fn default() -> Self {
        Self {
            select: true,
            navigate: true,
            scroll: true,
            copy_on_select: false,
        }
    }
}

/// What the app asks of the terminal, which only the event loop owns.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Effect {
    /// Turn mouse capture on or off.
    MouseCapture(bool),
    /// Put the text on the clipboard.
    Copy(String),
    /// Open the URL in the browser.
    OpenUrl {
        url: String,
        /// How often the mouse's notice had changed when the URL was clicked, so a failure to
        /// open it says so only while nothing has changed the notice since.
        notice_changes: u64,
    },
    /// Give the terminal back and stop until the shell continues the process.
    Suspend,
}

/// Errors from running or rendering the terminal interface.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The terminal backend failed.
    #[error(transparent)]
    Terminal(#[from] std::io::Error),
}

/// Terminal application state.
pub struct App {
    /// Running state of the TUI application.
    running: bool,
    input: Option<TerminalControl>,
    pending_pty_input: VecDeque<PreparedPtyInput>,
    input_notice: Option<String>,
    terminal_input_closed: bool,
    /// Prevents coalesced change notifications from hiding an intervening input-drop event.
    event_cursors: HashMap<String, u64>,
    shutdown: micromux::CancellationToken,
    /// Synchronous session data and lifecycle commands.
    source: SessionSource,
    /// Liveness-only change notifications; the TUI re-queries the model for content.
    changes: broadcast::Receiver<SessionChange>,
    /// Changes to the local session's URLs and routing status, which the model does not carry.
    summary_changes: Option<tokio::sync::watch::Receiver<micromux::SessionSummary>>,
    /// Where the threads that wait for URL openers report the ones that exited with an error.
    opener_failures: tokio::sync::mpsc::UnboundedSender<OpenerFailure>,
    /// The openers that exited with an error, for the event loop to say so.
    failed_openers: tokio::sync::mpsc::UnboundedReceiver<OpenerFailure>,
    /// Event handler
    input_event_handler: event::InputHandler,
    /// Current state
    state: state::State,
    /// Log viewer
    log_view: crate::render::log_view::LogView,
    /// Scrolling of the side pane, whichever one shows.
    side_view: crate::render::log_view::LogView,
    /// The pane beside the logs, if any.
    side_pane: Option<SidePane>,
    pretty_json_logs: bool,
    level_picker: Option<level::LevelPicker>,
    /// The search of the log pane.
    search: search::LogSearch,
    show_help: bool,
    /// The details popup on screen, if any.
    info: Option<InfoView>,
    /// First row of the help or details popup on screen, clamped when it renders.
    popup_scroll: usize,
    mouse: mouse::MouseState,
    /// Index of the service on the services list's first row in the last frame.
    first_service: usize,
    /// When a Ctrl-C last had the footer say how to quit, while it says so.
    quit_hint_at: Option<std::time::Instant>,
    /// Where the last frame drew what the mouse can act on.
    hits: mouse::Hits,
    /// Terminal work queued for the event loop.
    effects: Vec<Effect>,
    pty_input_mode: bool,
    /// Whether the log pane goes back to every service's logs when PTY input mode ends, which
    /// shows the selected service's logs while it lasts.
    all_logs_after_input: bool,
    /// Whether the PTYs follow this TUI's pane only while it types into a service, as an attached
    /// TUI's do.
    resizes_only_in_pty_input_mode: bool,
    focus: Focus,
    terminal_cols: u16,
    terminal_rows: u16,
    last_pty_cols: u16,
    last_pty_rows: u16,
}

/// Which details popup is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InfoView {
    /// Everything about the selected service.
    Service,
    /// The session's name, proxy, routes and allocated ports.
    Session,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// Service list is focused.
    Services,
    /// Log pane is focused.
    Logs,
    /// The side pane is focused.
    Side,
}

/// What the pane beside the logs shows; one at a time, so the logs keep half the width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidePane {
    /// The selected service's healthcheck attempts.
    Health,
    /// The selected service's lifecycle events.
    Events,
}

impl App {
    /// Construct a new [`App`] over a synchronous session source.
    #[must_use]
    pub fn new(
        source: SessionSource,
        input: Option<TerminalControl>,
        shutdown: micromux::CancellationToken,
        view_options: ViewOptions,
    ) -> Self {
        let changes = source.subscribe();
        let summary_changes = source.summary_changes();
        let resizes_only_in_pty_input_mode = matches!(source, SessionSource::Remote(_));
        let snapshots = source.services();
        let event_cursors = snapshots
            .iter()
            .filter_map(|snapshot| {
                source
                    .latest_event_seq(&snapshot.id)
                    .map(|seq| (snapshot.id.clone(), seq))
            })
            .collect();

        let services = snapshots.into_iter().map(state::Service::new).collect();

        let log_view = render::log_view::LogView::default();
        let side_view = render::log_view::LogView::default();

        let (opener_failures, failed_openers) = tokio::sync::mpsc::unbounded_channel();
        Self {
            running: true,
            input,
            pending_pty_input: VecDeque::new(),
            input_notice: None,
            terminal_input_closed: false,
            event_cursors,
            shutdown,
            source,
            changes,
            summary_changes,
            opener_failures,
            failed_openers,
            input_event_handler: event::InputHandler::new(),
            state: state::State::new(services),
            log_view,
            side_view,
            side_pane: None,
            pretty_json_logs: view_options.pretty_json,
            level_picker: None,
            search: search::LogSearch::default(),
            show_help: false,
            info: None,
            popup_scroll: 0,
            mouse: mouse::MouseState::new(view_options.mouse),
            first_service: 0,
            quit_hint_at: None,
            hits: mouse::Hits::default(),
            effects: Vec::new(),
            pty_input_mode: false,
            all_logs_after_input: false,
            resizes_only_in_pty_input_mode,
            focus: Focus::Services,
            terminal_cols: 80,
            terminal_rows: 24,
            last_pty_cols: 0,
            last_pty_rows: 0,
        }
    }
}

impl App {
    /// The size of the logs pane's text, which the service PTYs match.
    fn desired_pty_size(&self) -> (u16, u16) {
        let area = ratatui::layout::Rect::new(0, 0, self.terminal_cols, self.terminal_rows);
        let layout = self.screen_layout(area, self.chrome_rows(self.terminal_cols));
        let text = render::log_view::text_area(layout.logs);
        (text.width.max(1), text.height.max(1))
    }

    /// Sends the pane size to the service PTYs when it changed since the last resize sent.
    ///
    /// The last active writer wins: an attached TUI resizes only while it types into a service,
    /// so watching a session never changes its PTYs, while the session's own TUI follows its pane
    /// whenever it changes.
    fn maybe_resize_pty(&mut self) {
        let Some(input) = &self.input else {
            return;
        };
        if self.resizes_only_in_pty_input_mode && !self.pty_input_mode {
            return;
        }
        let (cols, rows) = self.desired_pty_size();
        if cols == self.last_pty_cols && rows == self.last_pty_rows {
            return;
        }

        // Only advance the dedup cache once the resize is actually queued; otherwise a dropped
        // ResizeAll (full channel) would leave the cache claiming the new size and never retry.
        if input.try_resize(cols, rows) {
            self.last_pty_cols = cols;
            self.last_pty_rows = rows;
        }
    }

    fn enqueue_pty_input(&mut self, service_id: String, bytes: Vec<u8>) {
        debug_assert!(
            bytes.len() <= micromux::MAX_PTY_INPUT_BATCH_BYTES,
            "keystroke input must fit one PTY batch"
        );
        let Some(input) = self.input.clone() else {
            return;
        };
        let prepared = match input.prepare_input(service_id, bytes) {
            Ok(prepared) => prepared,
            Err(err) => {
                self.report_input_prepare_error(err);
                return;
            }
        };
        if self.pending_pty_input.is_empty() {
            match input.try_send(prepared) {
                Ok(()) => self.input_notice = None,
                Err(PtyInputSendError::Full(prepared)) => {
                    self.push_pending_pty_input(prepared);
                    self.input_notice = None;
                }
                Err(PtyInputSendError::Closed(_)) => {
                    tracing::warn!("PTY input channel closed; discarding terminal input");
                }
                Err(PtyInputSendError::WrongSession(_)) => {
                    tracing::error!("PTY input reservation belongs to another session");
                }
                Err(PtyInputSendError::Disconnected(_)) => self.discard_disconnected_input(),
            }
        } else {
            self.push_pending_pty_input(prepared);
            self.input_notice = None;
        }
    }

    fn enqueue_pty_paste(&mut self, service_id: &str, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let Some(input) = self.input.clone() else {
            return;
        };
        let prepared = match input.prepare_paste(service_id.to_string(), bytes.to_vec()) {
            Ok(prepared) => prepared,
            Err(err) => {
                self.report_input_prepare_error(err);
                return;
            }
        };
        if self.pending_pty_input.is_empty() {
            match input.try_send(prepared) {
                Ok(()) => {
                    self.input_notice = None;
                    return;
                }
                Err(PtyInputSendError::Closed(_)) => {
                    tracing::warn!("PTY input channel closed; discarding pasted terminal input");
                    return;
                }
                Err(PtyInputSendError::WrongSession(_)) => {
                    tracing::error!("PTY paste reservation belongs to another session");
                    return;
                }
                Err(PtyInputSendError::Disconnected(_)) => {
                    self.discard_disconnected_input();
                    return;
                }
                Err(PtyInputSendError::Full(prepared)) => {
                    self.pending_pty_input.push_back(prepared);
                    self.input_notice = None;
                    return;
                }
            }
        }
        self.pending_pty_input.push_back(prepared);
        self.input_notice = None;
    }

    fn report_input_prepare_error(&mut self, error: PtyInputPrepareError) {
        let (kind, bytes, max_bytes) = match error {
            PtyInputPrepareError::TooLarge {
                kind,
                bytes,
                max_bytes,
            }
            | PtyInputPrepareError::BudgetExhausted {
                kind,
                bytes,
                max_bytes,
            }
            | PtyInputPrepareError::ServiceBudgetExhausted {
                kind,
                bytes,
                max_bytes,
            } => (kind, bytes, max_bytes),
        };
        let kind = kind.as_str();
        self.input_notice = Some(match error {
            PtyInputPrepareError::TooLarge { .. } => format!(
                "{kind} discarded: input exceeds the {} limit",
                format_byte_limit(max_bytes)
            ),
            PtyInputPrepareError::BudgetExhausted { .. } => {
                format!("{kind} discarded: terminal input backlog is full")
            }
            PtyInputPrepareError::ServiceBudgetExhausted { .. } => {
                format!("{kind} discarded: this service's input backlog is full")
            }
        });
        tracing::warn!(
            input_kind = kind,
            input_bytes = bytes,
            input_limit_bytes = max_bytes,
            ?error,
            "PTY input could not be retained; discarding it atomically"
        );
    }

    fn push_pending_pty_input(&mut self, prepared: PreparedPtyInput) {
        if let Some(queued) = self.pending_pty_input.back_mut() {
            if let Err(prepared) = queued.try_merge(prepared) {
                self.pending_pty_input.push_back(prepared);
            }
        } else {
            self.pending_pty_input.push_back(prepared);
        }
    }

    fn flush_pending_pty_input(&mut self) {
        let Some(input) = &self.input else {
            self.pending_pty_input.clear();
            return;
        };
        while let Some(prepared) = self.pending_pty_input.pop_front() {
            match input.try_send(prepared) {
                Ok(()) => {
                    self.input_notice = None;
                }
                Err(PtyInputSendError::Full(prepared)) => {
                    self.pending_pty_input.push_front(prepared);
                    break;
                }
                Err(PtyInputSendError::Closed(_)) => {
                    tracing::warn!("PTY input channel closed; discarding queued terminal input");
                    self.pending_pty_input.clear();
                    break;
                }
                Err(PtyInputSendError::WrongSession(_)) => {
                    tracing::error!("queued PTY input reservation belongs to another session");
                    self.pending_pty_input.clear();
                    break;
                }
                Err(PtyInputSendError::Disconnected(_)) => {
                    self.discard_disconnected_input();
                    break;
                }
            }
        }
    }

    /// Drops input that an attached session cannot take while it reconnects, together with
    /// everything queued behind it, so none of it reaches the session once it is back.
    fn discard_disconnected_input(&mut self) {
        self.pending_pty_input.clear();
        self.input_notice = Some(DISCONNECTED_INPUT_NOTICE.to_string());
    }

    /// Waits for what wakes the event loop next: input, a change to the session, a timer, a
    /// continue after a stop, or an opener that exited with an error; `None` once the session
    /// ends.
    async fn next_wake(&mut self, continued: &mut platform::Continued) -> Option<Wake> {
        let timer = self.next_timer();
        tokio::select! {
            () = self.shutdown.cancelled() => None,
            input = self.input_event_handler.next() => Some(Wake::Input(input)),
            change = self.changes.recv() => match change {
                Ok(change) => Some(Wake::Change(change)),
                Err(broadcast::error::RecvError::Lagged(_)) => Some(Wake::Resync),
                Err(broadcast::error::RecvError::Closed) => None,
            },
            () = async {
                match timer {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending::<()>().await,
                }
            } => Some(Wake::Timer),
            () = async {
                let changed = match self.summary_changes.as_mut() {
                    Some(summary) => summary.changed().await.is_ok(),
                    None => false,
                };
                if !changed {
                    std::future::pending::<()>().await;
                }
            } => Some(Wake::Summary),
            () = self.source.local_notice_changed() => Some(Wake::Summary),
            () = continued.recv() => Some(Wake::Continued),
            Some(failure) = self.failed_openers.recv() => Some(Wake::OpenerFailed(failure)),
        }
    }

    /// Run the TUI event loop.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying terminal backend fails to draw.
    pub async fn run(mut self, mut terminal: DefaultTerminal) -> Result<(), Error> {
        let mut continued = platform::Continued::watch();

        let area = terminal.size()?;
        self.terminal_cols = area.width;
        self.terminal_rows = area.height;
        self.state
            .clamp_sidebar(self.terminal_cols.saturating_sub(22));
        self.maybe_resize_pty();

        // Seed the view from the model before the first frame.
        self.resync();
        self.draw(&mut terminal)?;
        let mut last_frame = tokio::time::Instant::now();

        while self.is_running() {
            let wake = self.next_wake(&mut continued).await;

            let mut needs_resync = false;
            match wake {
                Some(Wake::Input(event)) => self.handle_input_event(event),
                Some(Wake::Change(change)) => needs_resync |= self.apply_change(&change),
                Some(Wake::Resync) => needs_resync = true,
                Some(Wake::Timer) => self.on_timer(std::time::Instant::now()),
                Some(Wake::Summary) => {}
                Some(Wake::Continued) => self.take_terminal_again(&mut terminal)?,
                Some(Wake::OpenerFailed(failure)) => {
                    self.opener_failed(&failure, std::time::Instant::now());
                }
                None => {
                    self.running = false;
                    continue;
                }
            }
            while self.running {
                let Some(event) = self.input_event_handler.try_next() else {
                    break;
                };
                self.handle_input_event(event);
            }
            loop {
                match self.changes.try_recv() {
                    Ok(change) => needs_resync |= self.apply_change(&change),
                    Err(broadcast::error::TryRecvError::Lagged(_)) => needs_resync = true,
                    Err(
                        broadcast::error::TryRecvError::Empty
                        | broadcast::error::TryRecvError::Closed,
                    ) => break,
                }
            }
            // PTY input mode lets the terminal send a service's keys as it would without micromux.
            terminal::set_keyboard_reporting(!self.pty_input_mode)?;
            if self.apply_effects(&mut terminal)? {
                self.suspend(&mut terminal, &mut continued).await?;
            }
            if needs_resync {
                self.resync();
            }
            // Header and footer rows follow their wrapped content, so any state change can move
            // the pane size the service PTYs must match.
            self.maybe_resize_pty();
            let frame_deadline = last_frame + FRAME_INTERVAL;
            if tokio::time::Instant::now() < frame_deadline {
                tokio::time::sleep_until(frame_deadline).await;
            }
            self.draw(&mut terminal)?;
            last_frame = tokio::time::Instant::now();
        }
        Ok(())
    }

    /// Draws a frame, and once more when the frame took away a selection whose text the panes
    /// no longer show, since the footer, drawn before the panes, still offered to copy it.
    fn draw<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut ratatui::Terminal<B>,
    ) -> Result<(), B::Error> {
        let selected = self.mouse.selection.is_some();
        terminal.draw(|frame| frame.render_widget(&mut *self, frame.area()))?;
        if selected && self.mouse.selection.is_none() {
            terminal.draw(|frame| frame.render_widget(&mut *self, frame.area()))?;
        }
        Ok(())
    }

    /// When the event loop must wake by itself: to retry queued PTY input, or to take back the
    /// quit hint or the mouse's notice.
    fn next_timer(&self) -> Option<std::time::Instant> {
        let retry = (!self.pending_pty_input.is_empty())
            .then(|| std::time::Instant::now() + PENDING_PTY_INPUT_RETRY);
        let hint = self.quit_hint_at.map(|at| at + QUIT_HINT_DURATION);
        let notice = self.mouse.notice_deadline();
        retry.into_iter().chain(hint).chain(notice).min()
    }

    /// Does what is due `now`: retries queued PTY input and takes back an expired quit hint or
    /// mouse notice.
    fn on_timer(&mut self, now: std::time::Instant) {
        if !self.pending_pty_input.is_empty() {
            self.flush_pending_pty_input();
        }
        if self
            .quit_hint_at
            .is_some_and(|at| now.saturating_duration_since(at) >= QUIT_HINT_DURATION)
        {
            self.quit_hint_at = None;
        }
        if self
            .mouse
            .notice_deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            self.mouse.set_notice(None, now);
        }
    }

    /// Ctrl-C pressed `now`, which never quits: only `q` does, so a stray Ctrl-C, as from a copy
    /// shortcut a terminal sends as Ctrl-C, cannot stop the session. It closes any popup and the
    /// search prompt, where `q` means something else, and the footer says for a moment that `q`
    /// quits.
    fn ctrl_c(&mut self, now: std::time::Instant) {
        self.show_help = false;
        self.info = None;
        self.level_picker = None;
        self.search.prompt = None;
        self.quit_hint_at = Some(now);
    }

    /// Does the terminal work that input queued, and returns whether a suspend was asked for,
    /// which the caller does last.
    fn apply_effects(&mut self, terminal: &mut DefaultTerminal) -> Result<bool, Error> {
        use crossterm::clipboard::CopyToClipboard;

        let mut suspend = false;
        for effect in std::mem::take(&mut self.effects) {
            match effect {
                Effect::MouseCapture(enabled) => terminal::set_mouse_capture(enabled)?,
                // Both the clipboard and Linux's primary selection, pasted with the middle button.
                Effect::Copy(text) => {
                    crossterm::execute!(
                        terminal.backend_mut(),
                        CopyToClipboard::to_clipboard_from(text.as_bytes()),
                        CopyToClipboard::to_primary_from(text.as_bytes())
                    )?;
                    clipboard::copy_locally(text);
                }
                Effect::OpenUrl {
                    url,
                    notice_changes,
                } => {
                    let now = std::time::Instant::now();
                    match open_url(&url) {
                        Ok(opener) => reap_opener(
                            opener,
                            OpenerFailure {
                                url,
                                notice_changes,
                                clicked_at: now,
                            },
                            self.opener_failures.clone(),
                        ),
                        Err(err) => {
                            tracing::warn!(?err, url, "cannot open the URL in a browser");
                            self.url_open_failed(&url, notice_changes, now);
                        }
                    }
                }
                Effect::Suspend => suspend = true,
            }
        }
        Ok(suspend)
    }

    /// Says, `now`, that `failure`'s opener exited with an error, if it did so soon after its click.
    fn opener_failed(&mut self, failure: &OpenerFailure, now: std::time::Instant) {
        if now.saturating_duration_since(failure.clicked_at) <= OPENER_FAILURE_WINDOW {
            self.url_open_failed(&failure.url, failure.notice_changes, now);
        }
    }

    /// Suspends to the shell as Ctrl-Z does there, then takes the terminal again once the shell
    /// continues micromux.
    async fn suspend(
        &mut self,
        terminal: &mut DefaultTerminal,
        continued: &mut platform::Continued,
    ) -> Result<(), Error> {
        /// How long to wait for the stop to land before taking the terminal back regardless, as
        /// when nothing could ever continue the process and the system ignores the stop.
        const STOP_LANDS_WITHIN: Duration = Duration::from_millis(100);

        terminal::stop_process_group()?;
        // The stop can reach another thread after the signal is sent, so wait for the process to
        // be continued rather than take the terminal while it is about to stop.
        let _ = tokio::time::timeout(STOP_LANDS_WITHIN, continued.recv()).await;
        self.take_terminal_again(terminal)
    }

    /// Takes the terminal over again after the process continued from a stop, and has the next
    /// frame draw every cell at the terminal's current size.
    fn take_terminal_again(&mut self, terminal: &mut DefaultTerminal) -> Result<(), Error> {
        use crossterm::terminal::{Clear, ClearType};

        terminal::take_over(self.mouse.capture, !self.pty_input_mode)?;
        // A new terminal forgets what was drawn; ratatui's own `clear` would ask the terminal
        // where its cursor is, an answer the input reader takes first.
        *terminal =
            ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
        crossterm::execute!(terminal.backend_mut(), Clear(ClearType::All))?;
        let size = terminal.size()?;
        self.handle_crossterm_event(&crossterm::event::Event::Resize(size.width, size.height));
        Ok(())
    }

    /// Apply a single liveness notification by re-querying the model for the affected service. The
    /// broadcast carries only `{service_id, kind}`; the content lives in the model.
    fn apply_change(&mut self, change: &SessionChange) -> bool {
        match change.kind {
            ChangeKind::Status => {
                if let Some(snapshot) = self.source.service(&change.service_id)
                    && let Some(service) = self.service_mut(&change.service_id)
                {
                    let renamed = service.snapshot.name != snapshot.name;
                    service.snapshot = snapshot;
                    if renamed && let Some(all_logs) = &mut self.state.all_logs {
                        all_logs.dirty = true;
                    }
                }
            }
            ChangeKind::Logs => {
                if let Some(service) = self.service_mut(&change.service_id) {
                    service.logs_dirty = true;
                }
            }
            ChangeKind::Health => {
                if let Some(service) = self.service_mut(&change.service_id) {
                    service.healthcheck_dirty = true;
                }
            }
            ChangeKind::Roster | ChangeKind::Unknown => return true,
            ChangeKind::Events => {
                self.observe_input_events(&change.service_id);
                if let Some(service) = self.service_mut(&change.service_id) {
                    service.events_dirty = true;
                }
            }
            ChangeKind::Heartbeat => {}
        }
        false
    }

    fn observe_input_events(&mut self, service_id: &str) {
        let after = self.event_cursors.get(service_id).copied();
        let (cursor, notice) = self.source.input_drop_notice_since(service_id, after);
        if let Some(cursor) = cursor {
            self.event_cursors.insert(service_id.to_string(), cursor);
        } else {
            self.event_cursors.remove(service_id);
        }
        if let Some(notice) = notice {
            self.input_notice = Some(notice);
        }
    }

    /// Reconcile the roster while retaining render caches for services that still exist.
    fn resync(&mut self) {
        let selected_id = self
            .state
            .current_service()
            .map(|service| service.snapshot.id.clone());
        let previous_index = self.state.selected_service;
        let mut existing = std::mem::take(&mut self.state.services)
            .into_iter()
            .map(|service| (service.snapshot.id.clone(), service))
            .collect::<std::collections::HashMap<_, _>>();
        self.state.services = self
            .source
            .services()
            .into_iter()
            .map(|snapshot| {
                if let Some(mut service) = existing.remove(&snapshot.id) {
                    service.snapshot = snapshot;
                    service.logs_dirty = true;
                    service.healthcheck_dirty = true;
                    service.events_dirty = true;
                    service
                } else {
                    state::Service::new(snapshot)
                }
            })
            .collect();
        let service_ids = self
            .state
            .services
            .iter()
            .map(|service| service.snapshot.id.clone())
            .collect::<Vec<_>>();
        let retained_ids = service_ids.iter().cloned().collect::<HashSet<_>>();
        self.event_cursors
            .retain(|service_id, _| retained_ids.contains(service_id));
        for service_id in service_ids {
            self.observe_input_events(&service_id);
        }
        self.state.selected_service = selected_id
            .and_then(|selected_id| {
                self.state
                    .services
                    .iter()
                    .position(|service| service.snapshot.id == selected_id)
            })
            .unwrap_or_else(|| previous_index.min(self.state.services.len().saturating_sub(1)));
        // Services may have come, gone or moved, which changes the order and tags of all logs.
        if let Some(all_logs) = &mut self.state.all_logs {
            all_logs.dirty = true;
        }
    }

    fn service_mut(&mut self, id: &str) -> Option<&mut state::Service> {
        self.state
            .services
            .iter_mut()
            .find(|service| service.snapshot.id == id)
    }

    fn handle_input_event(&mut self, input_event: event::Input) {
        match input_event {
            event::Input::Event(event) => self.handle_crossterm_event(&event),
            event::Input::Mouse { event, at } => self.handle_mouse(event, at),
            event::Input::Closed => {
                if !self.terminal_input_closed {
                    tracing::warn!("terminal input closed; supervision will continue");
                    self.terminal_input_closed = true;
                }
            }
        }
    }

    fn handle_crossterm_event(&mut self, event: &crossterm::event::Event) {
        use crossterm::event::{KeyCode, KeyEventKind};

        if let crossterm::event::Event::Key(key) = *event
            && terminal::debug_keys()
        {
            // What is typed into a service, a password among it, stays out of the log.
            let code = match key.code {
                KeyCode::Char(_) if self.pty_input_mode => "Char(…)".to_string(),
                code => format!("{code:?}"),
            };
            tracing::warn!(
                target: "micromux::keys",
                code,
                modifiers = ?key.modifiers,
                kind = ?key.kind,
                state = ?key.state,
                pty_input_mode = self.pty_input_mode,
                "key"
            );
        }
        match *event {
            crossterm::event::Event::Resize(cols, rows) => {
                self.terminal_cols = cols;
                self.terminal_rows = rows;
                self.state.clamp_sidebar(cols.saturating_sub(22));
                self.maybe_resize_pty();
            }
            crossterm::event::Event::Key(key) if key.kind == KeyEventKind::Press => {
                self.mouse.set_notice(None, std::time::Instant::now());
                self.handle_key_press(key);
            }
            crossterm::event::Event::Mouse(mouse) => {
                self.handle_mouse(mouse, std::time::Instant::now());
            }
            crossterm::event::Event::Paste(ref text) if self.pty_input_mode => {
                if let Some(service) = self.state.current_service() {
                    let service_id = service.snapshot.id.clone();
                    self.enqueue_pty_paste(&service_id, text.as_bytes());
                }
            }
            crossterm::event::Event::Paste(ref text) => self.paste_into_search_prompt(text),
            _ => {}
        }
    }

    fn handle_key_press(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        // PTY mode forwards every key to the selected process, Ctrl-C and the copy chords
        // included; command mode reserves Ctrl-C for leaving the TUI.
        if self.pty_input_mode {
            self.handle_key_press_pty_input_mode(key);
            return;
        }

        if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
            self.ctrl_c(std::time::Instant::now());
            return;
        }
        if is_copy_chord(key) {
            self.copy_selection();
            return;
        }
        // Only Unix shells can suspend a job.
        if cfg!(unix) && key.code == KeyCode::Char('z') && key.modifiers == KeyModifiers::CONTROL {
            self.effects.push(Effect::Suspend);
            return;
        }
        // The prompt takes characters typed with Alt and AltGr as well, which Windows reports as
        // Ctrl+Alt, so `@`, `{` or `\` can be typed on layouts that put them there; only Ctrl
        // chords stay out of the query.
        if self.search.prompt.is_some() {
            let control_chord = key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT);
            if !control_chord {
                self.handle_key_press_search_prompt(key.code);
            }
            return;
        }
        if key.modifiers != KeyModifiers::NONE && key.modifiers != KeyModifiers::SHIFT {
            return;
        }
        if self.show_help || self.info.is_some() {
            self.handle_key_press_popup(key.code);
            return;
        }
        if self.level_picker.is_some() {
            self.handle_key_press_level_picker(key.code);
            return;
        }

        match key.code {
            // Quit
            KeyCode::Char('q') if key.modifiers == KeyModifiers::NONE => self.exit(),

            // Drop the mouse selection, else the search
            KeyCode::Esc if self.mouse.selection.is_some() => self.mouse.selection = None,
            KeyCode::Esc => self.search.clear(),

            // Copy the mouse selection
            KeyCode::Char('y') => self.copy_selection(),
            KeyCode::Enter if self.has_selection() => self.copy_selection(),

            // Toggle focus
            KeyCode::Tab => self.toggle_focus(),

            // Toggle mouse capture
            KeyCode::Char('m') => self.toggle_mouse_capture(),

            // Enter PTY input mode
            KeyCode::Char('a') if self.input.is_some() => self.enter_pty_input_mode(),

            // Show a side pane, or hide it
            KeyCode::Char('H') => self.toggle_side_pane(SidePane::Health),
            KeyCode::Char('E') => self.toggle_side_pane(SidePane::Events),

            // Show every service's logs, or the selected service's again
            KeyCode::Char('A') => self.show_all_logs(self.state.all_logs.is_none()),

            // Disable current service
            KeyCode::Char('d') => self.disable_current_service(),

            KeyCode::Char('s') => self.stop_current_dynamic_service(),

            // Restart service
            KeyCode::Char('r') => self.restart_current_service(),

            // Restart all services
            KeyCode::Char('R') => self.restart_all_services(),

            // Navigation
            KeyCode::Char('k') | KeyCode::Up => self.navigate_up(),
            KeyCode::Char('j') | KeyCode::Down => self.navigate_down(),
            KeyCode::Char('g') => self.scroll_to_top(),
            KeyCode::Char('G') => self.scroll_to_bottom(),

            // Decrease service sidebar width (resize to the left)
            KeyCode::Char('-' | 'h') | KeyCode::Left => {
                self.state.resize_left();
                self.maybe_resize_pty();
            }

            // Increase service sidebar width (resize to the right)
            KeyCode::Char('+' | 'l') | KeyCode::Right => {
                // Keep enough room for a useful PTY after the sidebar and pane borders.
                self.state
                    .resize_right(self.terminal_cols.saturating_sub(22));
                self.maybe_resize_pty();
            }

            // Toggle wrapping for log viewer
            KeyCode::Char('w') => self.toggle_wrap(),

            // Toggle automatic tailing for log viewer
            KeyCode::Char('t') => self.toggle_tail(),

            // Search the logs
            KeyCode::Char('/') => self.open_search_prompt(),
            KeyCode::Char('n') if self.search.is_active() => {
                self.jump_to_match(search::Direction::Older);
            }
            KeyCode::Char('N') if self.search.is_active() => {
                self.jump_to_match(search::Direction::Newer);
            }

            KeyCode::Char('?') => {
                self.show_help = true;
                self.popup_scroll = 0;
            }
            // Without a selected service there is nothing to describe.
            KeyCode::Char('i') if self.state.current_service().is_some() => {
                self.open_info(InfoView::Service);
            }
            KeyCode::Char('u') => self.open_info(InfoView::Session),

            // Log presentation
            KeyCode::Char('L') => self.open_level_picker(),
            KeyCode::Char('T') => {
                if let Some(service) = self.state.current_service_mut() {
                    service.timestamps_override = Some(!service.shows_timestamps());
                }
            }
            KeyCode::Char('F') => {
                if let Some(service) = self.state.current_service_mut() {
                    service.filter_fields_override = Some(!service.filters_fields());
                }
            }
            _ => {}
        }
    }

    /// Keys while the help or a details popup shows, which swallow every other key so a stray
    /// press cannot act behind them: the popup's own key or Esc closes it, the other details
    /// popup's key switches to it, and the movement keys scroll it.
    fn handle_key_press_popup(&mut self, code: crossterm::event::KeyCode) {
        use crossterm::event::KeyCode;

        const PAGE: usize = 10;
        match (code, self.info) {
            (KeyCode::Esc, _) => {
                self.show_help = false;
                self.info = None;
            }
            (KeyCode::Char('?'), None) => self.show_help = false,
            (KeyCode::Char('y') | KeyCode::Enter, _) => self.copy_selection(),
            (KeyCode::Char('i'), Some(view)) => self.toggle_info(view, InfoView::Service),
            (KeyCode::Char('u'), Some(view)) => self.toggle_info(view, InfoView::Session),
            (KeyCode::Char('k') | KeyCode::Up, _) => {
                self.popup_scroll = self.popup_scroll.saturating_sub(1);
            }
            (KeyCode::Char('j') | KeyCode::Down, _) => {
                self.popup_scroll = self.popup_scroll.saturating_add(1);
            }
            (KeyCode::PageUp, _) => self.popup_scroll = self.popup_scroll.saturating_sub(PAGE),
            (KeyCode::PageDown, _) => self.popup_scroll = self.popup_scroll.saturating_add(PAGE),
            (KeyCode::Char('g'), _) => self.popup_scroll = 0,
            // Rendering clamps it to the last page.
            (KeyCode::Char('G'), _) => self.popup_scroll = usize::MAX,
            _ => {}
        }
    }

    fn open_info(&mut self, view: InfoView) {
        self.info = Some(view);
        self.popup_scroll = 0;
    }

    /// Closes `target` when it is the popup on screen, and opens it otherwise.
    fn toggle_info(&mut self, current: InfoView, target: InfoView) {
        if current == target {
            self.info = None;
        } else if target == InfoView::Session || self.state.current_service().is_some() {
            self.open_info(target);
        }
    }

    fn open_level_picker(&mut self) {
        let Some(service) = self.state.current_service() else {
            return;
        };
        self.level_picker = Some(level::LevelPicker::open(
            service.snapshot.id.clone(),
            service.min_level(),
        ));
    }

    fn handle_key_press_level_picker(&mut self, code: crossterm::event::KeyCode) {
        use crossterm::event::KeyCode;

        let Some(mut picker) = self.level_picker.take() else {
            return;
        };
        match code {
            KeyCode::Char('k') | KeyCode::Up => picker.up(),
            KeyCode::Char('j') | KeyCode::Down => picker.down(),
            KeyCode::Enter => {
                let min_level = picker.selected();
                if let Some(service) = self.service_mut(&picker.service_id) {
                    service.level_override = Some(level::LevelOverride { min_level });
                }
                return;
            }
            KeyCode::Esc | KeyCode::Char('q' | 'L') => return,
            _ => {}
        }
        self.level_picker = Some(picker);
    }

    /// Shows every service's logs in the log pane when `all` holds, else the selected service's,
    /// following the newest lines either way.
    fn show_all_logs(&mut self, all: bool) {
        if all == self.state.all_logs.is_some() {
            return;
        }
        self.state.all_logs = all.then(all_logs::AllLogs::new);
        self.log_view.follow_tail = true;
    }

    /// Starts typing into the selected service, and takes the PTY size back for this TUI's pane
    /// even when another client resized the PTYs since, because the client typing wins.
    ///
    /// The log pane shows the selected service while the mode lasts, so what is typed shows
    /// where it goes.
    fn enter_pty_input_mode(&mut self) {
        self.all_logs_after_input = self.state.all_logs.is_some();
        self.show_all_logs(false);
        self.pty_input_mode = true;
        self.last_pty_cols = 0;
        self.last_pty_rows = 0;
        self.maybe_resize_pty();
    }

    /// Stops typing into the selected service; an attached TUI also withdraws its size, so a
    /// reconnect to a replacement session cannot resize that session's PTYs while it only watches.
    fn leave_pty_input_mode(&mut self) {
        self.pty_input_mode = false;
        if std::mem::take(&mut self.all_logs_after_input) {
            self.show_all_logs(true);
        }
        if self.resizes_only_in_pty_input_mode
            && let Some(input) = &self.input
        {
            input.withdraw_size();
        }
    }

    fn handle_key_press_pty_input_mode(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        // Ctrl+], telnet's escape key, arrives apart from every other key in every terminal:
        // as `]` with Ctrl through the kitty protocol, and as the byte 0x1d, which crossterm
        // reads as Ctrl+5, without it. Alt+Esc leaves too, where the terminal reports it apart.
        let exit = match key.code {
            KeyCode::Char(']' | '5') => key.modifiers == KeyModifiers::CONTROL,
            KeyCode::Esc => key.modifiers.contains(KeyModifiers::ALT),
            _ => false,
        };
        match key.code {
            _ if exit => self.leave_pty_input_mode(),
            _ => {
                if let Some(bytes) = key_event_to_bytes(key.code, key.modifiers)
                    && let Some(service) = self.state.current_service()
                {
                    let service_id = service.snapshot.id.clone();
                    self.enqueue_pty_input(service_id, bytes);
                }
            }
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = if self.side_pane.is_some() {
            match self.focus {
                Focus::Services => Focus::Logs,
                Focus::Logs => Focus::Side,
                Focus::Side => Focus::Services,
            }
        } else {
            match self.focus {
                Focus::Services => Focus::Logs,
                Focus::Logs | Focus::Side => Focus::Services,
            }
        };
    }

    /// Hides `pane` when it shows, and shows it otherwise, in place of the other side pane if that
    /// one shows, following its newest lines.
    ///
    /// The log pane, which the service PTYs match, is as wide with either side pane.
    fn toggle_side_pane(&mut self, pane: SidePane) {
        if self.side_pane == Some(pane) {
            self.side_pane = None;
            if self.focus == Focus::Side {
                self.focus = Focus::Logs;
            }
        } else {
            self.side_pane = Some(pane);
            self.side_view.follow_tail = true;
        }
        self.maybe_resize_pty();
    }

    fn navigate_up(&mut self) {
        match self.focus {
            Focus::Services => self.state.service_up(),
            Focus::Logs => self.scroll_logs_up(1),
            Focus::Side => self.scroll_side_pane_up(1),
        }
    }

    fn navigate_down(&mut self) {
        match self.focus {
            Focus::Services => self.state.service_down(),
            Focus::Logs => self.scroll_logs_down(1),
            Focus::Side => self.scroll_side_pane_down(1),
        }
    }

    fn scroll_to_top(&mut self) {
        match self.focus {
            Focus::Logs => {
                self.log_view.follow_tail = false;
                self.log_view.scroll_offset = 0;
            }
            Focus::Side => {
                self.side_view.follow_tail = false;
                self.side_view.scroll_offset = 0;
            }
            Focus::Services => {}
        }
    }

    fn scroll_to_bottom(&mut self) {
        match self.focus {
            Focus::Logs => {
                self.log_view.follow_tail = true;
            }
            Focus::Side => {
                self.side_view.follow_tail = true;
            }
            Focus::Services => {}
        }
    }

    fn toggle_wrap(&mut self) {
        let wrap = !self.log_view.wrap;
        self.log_view.wrap = wrap;
        self.side_view.wrap = wrap;
    }

    fn toggle_tail(&mut self) {
        match self.focus {
            Focus::Logs | Focus::Services => {
                self.log_view.follow_tail = !self.log_view.follow_tail;
            }
            Focus::Side => {
                self.side_view.follow_tail = !self.side_view.follow_tail;
            }
        }
    }

    fn log_viewport_height(&self) -> u16 {
        // Total rows minus the header, the footer, and the logs block borders.
        let chrome = self.chrome_rows(self.terminal_cols);
        self.terminal_rows
            .saturating_sub(chrome.header)
            .saturating_sub(chrome.footer)
            .saturating_sub(2)
    }

    fn scroll_logs_up(&mut self, lines: u16) {
        self.scroll_logs(-isize::from(lines.cast_signed()));
    }

    fn scroll_logs_down(&mut self, lines: u16) {
        self.scroll_logs(isize::from(lines.cast_signed()));
    }

    /// Scrolls the logs by `rows`, up when negative, as [`render::log_view::LogView::scroll_by`]
    /// does.
    fn scroll_logs(&mut self, rows: isize) {
        let Some(shown) = self.state.shown_log() else {
            return;
        };
        // Wrap-aware count persisted by the renderer, so scrolling reaches the true bottom
        // even when wrapping is enabled (the raw entry count would stop short).
        let total = shown.log.line_index.total_lines();
        let viewport = usize::from(self.log_viewport_height());
        self.log_view.scroll_by(rows, total, viewport);
    }

    fn scroll_side_pane_up(&mut self, lines: u16) {
        self.scroll_side_pane(-isize::from(lines.cast_signed()));
    }

    fn scroll_side_pane_down(&mut self, lines: u16) {
        self.scroll_side_pane(isize::from(lines.cast_signed()));
    }

    /// Scrolls the side pane by `rows`, up when negative.
    fn scroll_side_pane(&mut self, rows: isize) {
        let (Some(service), Some(pane)) = (self.state.current_service(), self.side_pane) else {
            return;
        };
        let total = service.side_text(pane).line_index.total_lines();
        let viewport = usize::from(self.log_viewport_height());
        self.side_view.scroll_by(rows, total, viewport);
    }

    fn is_running(&self) -> bool {
        self.running
    }

    fn exit(&mut self) {
        // A remote source owns only its mirror task, so cancelling it cannot stop the session.
        self.source.cancel();
        self.shutdown.cancel();
        self.running = false;
    }

    /// Disable service
    fn disable_current_service(&self) {
        let Some(service) = self.state.current_service() else {
            return;
        };
        tracing::info!(service_id = service.snapshot.id, "disabling service");
        match service.snapshot.desired {
            micromux::Desired::Disabled => self.source.enable(service.snapshot.id.clone()),
            micromux::Desired::Enabled => self.source.disable(service.snapshot.id.clone()),
            micromux::Desired::Unknown => {}
        }
    }

    fn stop_current_dynamic_service(&self) {
        let Some(service) = self.state.current_service() else {
            return;
        };
        if service.snapshot.origin != micromux::OriginKind::Dynamic
            || service.snapshot.retired.is_some()
        {
            return;
        }
        tracing::info!(service_id = service.snapshot.id, "retiring dynamic service");
        self.source.stop_dynamic(service.snapshot.id.clone());
    }

    /// Restart service
    fn restart_current_service(&self) {
        let Some(service) = self.state.current_service() else {
            return;
        };
        tracing::info!(service_id = service.snapshot.id, "restarting service");
        self.source.restart(service.snapshot.id.clone());
    }

    /// Restart all services
    fn restart_all_services(&self) {
        tracing::info!("restarting all services");
        self.source.restart_all();
    }
}

/// Gives the terminal back if the TUI holds it, for an exit that skips the TUI's own teardown,
/// such as a second interrupt.
///
/// It releases mouse capture, leaves the alternate screen and turns raw mode off, each only if
/// it is on, so calling it again or without a TUI does nothing.
pub fn release_terminal() {
    // An exit path has nowhere better to report a failure to restore the terminal.
    terminal::give_back().ok();
}

/// What wakes the event loop.
#[derive(Debug)]
enum Wake {
    Input(event::Input),
    Change(SessionChange),
    /// The change broadcast lagged (or this is the initial draw): re-read everything.
    Resync,
    /// Queued PTY input is due for another try, or the quit prompt for one.
    Timer,
    /// The session's URLs or routing status, or the local notice, changed; only the header
    /// needs a redraw.
    Summary,
    /// The process continued after a stop, and the shell may have taken the terminal.
    Continued,
    /// A URL's opener exited with an error.
    OpenerFailed(OpenerFailure),
}

/// How long after its click an opener's error still says the URL could not be opened.
///
/// An opener that hands the URL to a running browser exits at once, but some of `xdg-open`'s
/// fallbacks run the browser in the foreground and exit with its status when it quits, which
/// says nothing about the click.
const OPENER_FAILURE_WINDOW: Duration = Duration::from_secs(10);

/// A URL whose opener exited with an error, and the click that opened it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenerFailure {
    url: String,
    /// How often the mouse's notice had changed when the URL was clicked.
    notice_changes: u64,
    clicked_at: std::time::Instant,
}

/// Waits for `opener` on a thread of its own, so it does not linger as a zombie, and reports
/// `failure` on `failures` when it exits with an error.
///
/// Windows' `explorer` exits with an error even when it opens the URL, so there only starting it
/// counts.
fn reap_opener(
    mut opener: std::process::Child,
    failure: OpenerFailure,
    failures: tokio::sync::mpsc::UnboundedSender<OpenerFailure>,
) {
    std::thread::spawn(move || {
        let failed = opener.wait().is_ok_and(|status| !status.success());
        if failed && !cfg!(windows) {
            // The TUI may have ended, which nothing needs to hear of.
            let _ = failures.send(failure);
        }
    });
}

/// Starts the platform's opener for `url`, without waiting for it, unless the TUI runs over ssh,
/// where the browser would open on the remote host, if anywhere.
///
/// The opener's output is discarded, since anything it prints would land on the TUI.
fn open_url(url: &str) -> std::io::Result<std::process::Child> {
    use std::process::{Command, Stdio};

    if clipboard::over_ssh() {
        return Err(std::io::Error::other("micromux runs over ssh"));
    }

    let opener = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let mut command = Command::new(opener);
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    platform::detach(&mut command);
    command.spawn()
}

/// Whether `key` is one of the terminals' standard copy shortcuts: Ctrl+Shift+C or Ctrl+Insert,
/// as on Linux and Windows, or Super+C, as Cmd+C on macOS.
///
/// Ctrl+Shift+C and Super+C only arrive through the kitty keyboard protocol; elsewhere the first
/// is sent as Ctrl-C, which never copies, and the second not at all.
fn is_copy_chord(key: crossterm::event::KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};

    // A terminal that reports the shifted key sends it without Shift.
    let (code, modifiers) = match key.code {
        KeyCode::Char(c) if c.is_ascii_uppercase() => (
            KeyCode::Char(c.to_ascii_lowercase()),
            key.modifiers | KeyModifiers::SHIFT,
        ),
        code => (code, key.modifiers),
    };
    matches!(
        (code, modifiers),
        (KeyCode::Char('c'), m) if m == KeyModifiers::CONTROL | KeyModifiers::SHIFT
            || m == KeyModifiers::SUPER
    ) || (code == KeyCode::Insert && modifiers == KeyModifiers::CONTROL)
}

/// The bytes a terminal without the kitty keyboard protocol sends for `code` with `modifiers`,
/// as xterm does, which is what a service in PTY input mode expects to read.
///
/// micromux may have asked its own terminal for the kitty protocol, or tmux for modifyOtherKeys,
/// which report keys such as Ctrl+Shift+C or Ctrl+I apart that legacy terminals send alike; this
/// folds them back into the legacy bytes. Super and the other modifiers legacy terminals cannot send are left out.
fn key_event_to_bytes(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Option<Vec<u8>> {
    use crossterm::event::{KeyCode, KeyModifiers};

    let shift = modifiers.contains(KeyModifiers::SHIFT);
    let alt = modifiers.contains(KeyModifiers::ALT);
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    // xterm's modifier parameter for cursor, editing and function keys.
    let parameter = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let cursor = |end: char| {
        if parameter == 1 {
            format!("\x1b[{end}")
        } else {
            format!("\x1b[1;{parameter}{end}")
        }
        .into_bytes()
    };
    let tilde = |number: u8| {
        if parameter == 1 {
            format!("\x1b[{number}~")
        } else {
            format!("\x1b[{number};{parameter}~")
        }
        .into_bytes()
    };
    // These carry their modifiers in the parameter rather than an Escape prefix.
    let special = match code {
        KeyCode::Up => Some(cursor('A')),
        KeyCode::Down => Some(cursor('B')),
        KeyCode::Right => Some(cursor('C')),
        KeyCode::Left => Some(cursor('D')),
        KeyCode::Home => Some(cursor('H')),
        KeyCode::End => Some(cursor('F')),
        KeyCode::Insert => Some(tilde(2)),
        KeyCode::Delete => Some(tilde(3)),
        KeyCode::PageUp => Some(tilde(5)),
        KeyCode::PageDown => Some(tilde(6)),
        KeyCode::F(number @ 1..=4) => {
            let end = char::from(b'P' + number - 1);
            Some(if parameter == 1 {
                format!("\x1bO{end}").into_bytes()
            } else {
                cursor(end)
            })
        }
        KeyCode::F(number @ 5..=12) => {
            const CODES: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];
            CODES
                .get(usize::from(number - 5))
                .map(|&number| tilde(number))
        }
        _ => None,
    };
    if special.is_some() {
        return special;
    }

    let mut bytes = match code {
        KeyCode::Esc => vec![0x1b],
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace if ctrl => vec![0x08],
        KeyCode::Backspace => vec![0x7f],
        // xterm sends the key itself where Ctrl has no control character for it.
        KeyCode::Char(c) if ctrl => {
            control_byte(c).map_or_else(|| c.to_string().into_bytes(), |byte| vec![byte])
        }
        // The kitty protocol reports Alt+Shift+A as a lowercase `a` with Shift.
        KeyCode::Char(c) if shift && c.is_ascii_lowercase() => {
            c.to_ascii_uppercase().to_string().into_bytes()
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        _ => return None,
    };
    if alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

/// The control character xterm sends for Ctrl with `c`, if it has one.
fn control_byte(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    if c.is_ascii_lowercase() {
        return u8::try_from(c).ok().map(|c| c - b'a' + 1);
    }
    Some(match c {
        ' ' | '@' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '~' | '6' => 0x1e,
        '_' | '/' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::path::Path;

    use super::*;
    use codespan_reporting::diagnostic::Diagnostic;
    use color_eyre::eyre;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use indoc::indoc;
    use micromux::{Command, TerminalSize};
    use ratatui::widgets::Widget as _;
    use similar_asserts::assert_eq;
    use tokio::sync::mpsc;

    /// A supervisor for `parsed` whose run-log spool goes to the test's own `cache` directory,
    /// never the user's.
    fn test_micromux(
        parsed: &micromux::ConfigFile<usize>,
        cache: &tempfile::TempDir,
    ) -> Result<micromux::Micromux, micromux::Error> {
        Ok(micromux::Micromux::new(parsed)?.with_cache_directory(cache.path()))
    }

    fn tab_press() -> crate::event::Input {
        crate::event::Input::Event(crossterm::event::Event::Key(KeyEvent {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }))
    }

    fn fill_terminal_queue(terminal: &TerminalControl) -> eyre::Result<usize> {
        let mut sent = 0;
        loop {
            let input = terminal.prepare_input("svc".to_string(), vec![b'x'])?;
            match terminal.try_send(input) {
                Ok(()) => sent += 1,
                Err(PtyInputSendError::Full(_)) => return Ok(sent),
                Err(PtyInputSendError::Closed(_)) => {
                    eyre::bail!("scheduler input channel closed")
                }
                Err(PtyInputSendError::WrongSession(_)) => {
                    eyre::bail!("test input reservation belonged to another session")
                }
                Err(PtyInputSendError::Disconnected(_)) => {
                    eyre::bail!("test terminal refused input as disconnected")
                }
            }
        }
    }

    #[test]
    fn ctrl_letters_map_to_ascii_control_codes() {
        assert_eq!(
            key_event_to_bytes(KeyCode::Char('a'), KeyModifiers::CONTROL),
            Some(vec![0x01])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Char('z'), KeyModifiers::CONTROL),
            Some(vec![0x1a])
        );
    }

    #[test]
    fn ctrl_specials_map_to_standard_codes() {
        assert_eq!(
            key_event_to_bytes(KeyCode::Char('@'), KeyModifiers::CONTROL),
            Some(vec![0x00])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Char('['), KeyModifiers::CONTROL),
            Some(vec![0x1b])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Char('\\'), KeyModifiers::CONTROL),
            Some(vec![0x1c])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Char(']'), KeyModifiers::CONTROL),
            Some(vec![0x1d])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Char('^'), KeyModifiers::CONTROL),
            Some(vec![0x1e])
        );
        assert_eq!(
            key_event_to_bytes(KeyCode::Char('_'), KeyModifiers::CONTROL),
            Some(vec![0x1f])
        );
    }

    #[tokio::test]
    async fn only_plain_q_quits_in_command_mode() -> eyre::Result<()> {
        let mut diagnostics = Vec::new();
        let parsed = micromux::from_str(
            "version: 1\nservices: {}\n",
            Path::new("."),
            0usize,
            None,
            &mut diagnostics,
        )?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let shutdown = micromux::CancellationToken::new();
        let (_runner, handles) = mux.start(shutdown.clone());
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal.clone()),
            shutdown.clone(),
            ViewOptions::default(),
        );
        let key = |code, modifiers| KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        app.handle_key_press(key(KeyCode::Char('q'), KeyModifiers::ALT));
        assert!(app.running);
        assert!(!shutdown.is_cancelled());

        app.handle_key_press(key(KeyCode::Char('H'), KeyModifiers::SHIFT));
        assert_eq!(app.side_pane, Some(SidePane::Health));

        // Ctrl-C never quits, however often it comes; plain `q` does.
        for _ in 0..3 {
            app.handle_key_press(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        }
        assert!(app.running);
        assert!(!shutdown.is_cancelled());
        app.handle_key_press(key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert!(!app.running);
        assert!(shutdown.is_cancelled());
        Ok(())
    }

    /// Ctrl-C never stops the session: the footer only says for a moment that `q` quits.
    #[tokio::test]
    async fn ctrl_c_only_says_that_q_quits() -> eyre::Result<()> {
        let mut diagnostics = Vec::new();
        let parsed = micromux::from_str(
            "version: 1\nservices:\n  svc:\n    command: [\"sleep\", \"30\"]\n",
            Path::new("."),
            0usize,
            None,
            &mut diagnostics,
        )?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let shutdown = micromux::CancellationToken::new();
        let (_runner, handles) = mux.start(shutdown.clone());
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal.clone()),
            shutdown.clone(),
            ViewOptions::default(),
        );
        let now = std::time::Instant::now();
        // Narrow enough that the hint may have no free room in the footer and cover its last
        // row. Every hint that takes clicks there still shows.
        let footer = |app: &mut App| {
            let area = ratatui::layout::Rect::new(0, 0, 40, 20);
            let mut buffer = ratatui::buffer::Buffer::empty(area);
            ratatui::widgets::Widget::render(&mut *app, area, &mut buffer);
            let row = (0..40)
                .filter_map(|x| buffer.cell((x, 19)).map(ratatui::buffer::Cell::symbol))
                .collect::<Vec<_>>();
            for hit in app.hits.hints.iter().filter(|hit| hit.area.y == 19) {
                let shown = row
                    .get(usize::from(hit.area.x)..)
                    .unwrap_or_default()
                    .concat();
                assert!(shown.starts_with(hit.keys), "{} under {shown:?}", hit.keys);
            }
            row.concat()
        };

        // A press says how to quit, without stopping anything, and closes a popup, where `q`
        // would not quit.
        app.show_help = true;
        app.ctrl_c(now);
        assert!(app.running);
        assert!(!app.show_help);
        assert!(footer(&mut app).contains("press q to quit"));
        app.handle_key_press(KeyEvent {
            code: KeyCode::Char('L'),
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        assert!(app.level_picker.is_some());
        app.ctrl_c(now);
        assert!(app.level_picker.is_none());

        // Once its moment passes, the hint goes; however often Ctrl-C comes, nothing stops.
        app.on_timer(now + QUIT_HINT_DURATION);
        assert!(!footer(&mut app).contains("press q"));
        for later in [3, 3, 4] {
            app.ctrl_c(now + Duration::from_secs(later));
        }
        assert!(app.running);
        assert!(!shutdown.is_cancelled());
        Ok(())
    }

    #[tokio::test]
    async fn log_presentation_keys_pick_a_level_and_toggle_timestamps_and_fields()
    -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["true"]
                logs:
                  level: warn
                  fields: {filename: hide}
              other:
                command: ["true"]
                logs:
                  filter_fields: false
                  fields: {filename: hide}
        "#};
        let mut diagnostics = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let shutdown = micromux::CancellationToken::new();
        let (_runner, handles) = mux.start(shutdown.clone());
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal),
            shutdown.clone(),
            ViewOptions::default(),
        );
        let press = |app: &mut App, code| {
            app.handle_key_press(KeyEvent {
                code,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            });
        };
        let render = |app: &mut App| {
            let area = ratatui::layout::Rect::new(0, 0, 200, 24);
            let mut buffer = ratatui::buffer::Buffer::empty(area);
            app.render(area, &mut buffer);
            buffer
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
        };

        // The configured threshold and hidden fields are active before any key press.
        let initial = render(&mut app);
        // The filters sit in the pane's top-right corner, apart from the pane name.
        assert!(initial.contains("┌Logs─"));
        assert!(initial.contains("─ level ≥ WARN · 1 field hidden ┐"));
        assert!(initial.contains("Level:WARN"));

        // The picker opens on the configured threshold, and `q` closes it instead of quitting.
        press(&mut app, KeyCode::Char('L'));
        assert!(render(&mut app).contains("Level · svc"));
        press(&mut app, KeyCode::Char('q'));
        assert!(app.level_picker.is_none());
        assert!(app.running);

        // Moving up from WARN and confirming overrides the configured threshold with INFO.
        press(&mut app, KeyCode::Char('L'));
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Enter);
        assert!(app.level_picker.is_none());
        assert_eq!(
            app.state
                .current_service()
                .map(crate::state::Service::min_level),
            Some(Some(micromux::StructuredLogLevel::Info))
        );

        // Timestamps start from the config, and both they and the hidden fields toggle for the
        // selected service.
        assert!(render(&mut app).contains("Time:ON"));
        press(&mut app, KeyCode::Char('T'));
        assert_eq!(
            app.state
                .current_service()
                .map(crate::state::Service::shows_timestamps),
            Some(false)
        );
        press(&mut app, KeyCode::Char('F'));
        let toggled = render(&mut app);
        assert!(toggled.contains("Time:OFF"));
        assert!(toggled.contains("Fields:ALL"));
        assert!(toggled.contains("─ level ≥ INFO ┐"));
        assert!(!toggled.contains("field hidden"));

        // Another service keeps its own configured level, timestamps, and field filtering, which
        // it starts with off.
        press(&mut app, KeyCode::Down);
        let other = render(&mut app);
        assert!(other.contains("Level:ALL"));
        assert!(other.contains("Time:ON"));
        assert!(other.contains("Fields:ALL"));
        assert!(!other.contains("field hidden"));
        press(&mut app, KeyCode::Char('F'));
        assert!(render(&mut app).contains("─ 1 field hidden ┐"));

        // The help overlay ignores other keys, even quit, until Esc closes it.
        press(&mut app, KeyCode::Char('?'));
        assert!(render(&mut app).contains("Log level threshold"));
        press(&mut app, KeyCode::Char('q'));
        assert!(app.show_help);
        assert!(app.running);
        press(&mut app, KeyCode::Esc);
        assert!(!app.show_help);

        Ok(())
    }

    #[tokio::test]
    async fn narrow_terminals_wrap_every_key_hint_and_shrink_the_pty() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["true"]
        "#};
        let mut diagnostics = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let (_runner, handles) = mux.start(micromux::CancellationToken::new());
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal),
            micromux::CancellationToken::new(),
            ViewOptions::default(),
        );

        // A wide terminal fits the status and the view controls on one header row and every action
        // on one footer row.
        app.terminal_cols = 240;
        app.terminal_rows = 40;
        let wide = app.chrome_rows(app.terminal_cols);
        assert_eq!((wide.header, wide.footer), (1, 1));
        let (_, wide_pty_rows) = app.desired_pty_size();

        // A narrow one wraps both rows instead of cutting hints off.
        app.terminal_cols = 60;
        let narrow = app.chrome_rows(app.terminal_cols);
        assert!(narrow.header > 1 && narrow.footer > 1, "{narrow:?}");
        let area = ratatui::layout::Rect::new(0, 0, app.terminal_cols, app.terminal_rows);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        (&mut app).render(area, &mut buffer);
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        for hint in [
            "micromux v",
            "Level:ALL",
            "Pane:OFF",
            "? Help",
            "d Disable",
            "PTY input",
            "q Quit",
        ] {
            assert!(rendered.contains(hint), "missing {hint:?}");
        }
        // The extra chrome rows come out of the service PTY, keeping it the size of the pane.
        let (_, narrow_pty_rows) = app.desired_pty_size();
        assert_eq!(
            wide_pty_rows - narrow_pty_rows,
            narrow.header + narrow.footer - 2
        );
        Ok(())
    }

    #[tokio::test]
    async fn header_status_lines_up_with_the_services_title() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["true"]
        "#};
        let mut diagnostics = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let (_runner, handles) = mux.start(micromux::CancellationToken::new());
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal),
            micromux::CancellationToken::new(),
            ViewOptions::default(),
        );
        let area = ratatui::layout::Rect::new(0, 0, 200, 24);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        (&mut app).render(area, &mut buffer);
        let row = |y: u16| {
            (0..area.width)
                .filter_map(|x| buffer.cell((x, y)).map(ratatui::buffer::Cell::symbol))
                .collect::<String>()
        };

        // Columns, not byte offsets: the box-drawing border characters are multi-byte.
        let column = |row: &str, needle: &str| {
            row.find(needle)
                .and_then(|byte| row.get(..byte))
                .map(|prefix| prefix.chars().count())
        };

        assert_eq!(column(&row(0), "micromux"), Some(1));
        assert_eq!(column(&row(1), "Services"), Some(1));
        Ok(())
    }

    /// A service row names the service only: its ports belong to the `i` popup and its URLs to
    /// the Logs title.
    #[tokio::test]
    async fn service_rows_leave_ports_to_the_details_popup() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              api:
                command: ["true"]
                ports: [8080, 9090]
        "#};
        let mut diagnostics = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let (_runner, handles) = mux.start(micromux::CancellationToken::new());
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal),
            micromux::CancellationToken::new(),
            ViewOptions::default(),
        );
        let area = ratatui::layout::Rect::new(0, 0, 200, 24);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        (&mut app).render(area, &mut buffer);
        let row = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .filter_map(|x| buffer.cell((x, y)).map(ratatui::buffer::Cell::symbol))
                    .collect::<String>()
            })
            .find(|row| row.contains(" api"))
            .ok_or_else(|| eyre::eyre!("no row lists the api service"))?;

        assert!(!row.contains(":8080"), "{row}");
        assert!(!row.contains('['), "{row}");
        Ok(())
    }

    /// The thread that waits for an opener reports it when it exits with an error, and not when
    /// it succeeds.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_opener_that_exits_with_an_error_is_reported() -> eyre::Result<()> {
        let failure = OpenerFailure {
            url: "http://app.localhost".to_string(),
            notice_changes: 7,
            clicked_at: std::time::Instant::now(),
        };
        for (program, reported) in [("false", Some(failure.clone())), ("true", None)] {
            let (failures, mut failed) = tokio::sync::mpsc::unbounded_channel();
            let opener = std::process::Command::new(program).spawn()?;
            reap_opener(opener, failure.clone(), failures);
            // The thread drops its sender once the opener exits, which ends the channel.
            let first = tokio::time::timeout(Duration::from_secs(10), failed.recv()).await?;
            assert_eq!(first, reported, "{program}");
        }
        Ok(())
    }

    /// Sessions with routes, whose services run `sleep` and `sh` as only Unix has them.
    #[cfg(unix)]
    mod routed {
        use super::*;
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use ratatui::layout::Position;
        use similar_asserts::assert_eq;

        /// A local app for a session established from `yaml` in `directory`, with its live session
        /// status and allocated ports.
        fn routed_app(
            directory: &Path,
            yaml: &str,
        ) -> eyre::Result<(
            App,
            micromux::SessionStatus,
            indexmap::IndexMap<String, u16>,
        )> {
            let config_path = directory.join("micromux.yaml");
            std::fs::write(&config_path, yaml)?;
            let mut parsed = micromux::from_str(yaml, directory, 0usize, None, &mut Vec::new())?;
            parsed.config_path = Some(config_path.canonicalize()?);
            let registry = micromux::PortRegistry::at(directory.join("registry"));
            let session = micromux::Session::establish(&parsed, &registry)?;
            let ports = session.ports().clone();
            let mux = std::sync::Arc::new(
                micromux::Micromux::with_session(&parsed, session)?
                    .with_cache_directory(directory.join("cache")),
            );
            let (_runner, handles) = mux.start(micromux::CancellationToken::new());
            let app = App::new(
                SessionSource::Local(
                    LocalSource::new(handles.reader, handles.commands.clone())
                        .with_session(handles.session.clone()),
                ),
                Some(handles.terminal),
                micromux::CancellationToken::new(),
                ViewOptions::default(),
            );
            Ok((app, handles.session, ports))
        }

        /// A session's URLs sit in the Logs title of the service each belongs to, never in the
        /// header, which only says when the proxy does not serve them.
        #[tokio::test]
        async fn logs_title_shows_the_selected_services_urls() -> eyre::Result<()> {
            let directory = tempfile::tempdir()?;
            let yaml = indoc! {r#"
                version: 1
                session: { name: tui }
                proxy: { port: 8080 }
                routes:
                  - host: app
                    port: 9997
                  - host: api
                    port: 9998
                  - host: admin
                    port: 9999
                services:
                  web:
                    command: ["sleep", "30"]
                    ports: [9997, 9998, 9999]
                    logs: { level: warn }
                  worker:
                    command: ["sleep", "30"]
            "#};
            let (mut app, session, _) = routed_app(directory.path(), yaml)?;
            // Every rendered row, the number of header rows, and the Logs title from its corner on.
            let render = |app: &mut App, width: u16| {
                let area = ratatui::layout::Rect::new(0, 0, width, 24);
                let mut buffer = ratatui::buffer::Buffer::empty(area);
                ratatui::widgets::Widget::render(&mut *app, area, &mut buffer);
                let rows = (0..area.height)
                    .map(|y| {
                        (0..width)
                            .filter_map(|x| buffer.cell((x, y)).map(ratatui::buffer::Cell::symbol))
                            .collect::<String>()
                    })
                    .collect::<Vec<_>>();
                let title = rows
                    .iter()
                    .find_map(|row| row.find("┌Logs").and_then(|at| row.get(at..)))
                    .map(str::to_string)
                    .unwrap_or_default();
                (rows, usize::from(app.chrome_rows(width).header), title)
            };
            // The header is the status row alone, and the service's URLs follow the title in order.
            let (rows, header, title) = render(&mut app, 200);
            assert_eq!(header, 1, "{rows:#?}");
            assert!(!rows[0].contains("http://"), "{}", rows[0]);
            assert!(
                title.starts_with(
                    "┌Logs · http://app.tui.localhost:8080 · http://api.tui.localhost:8080 · \
                     http://admin.tui.localhost:8080 ─"
                ),
                "{title}"
            );

            // A narrower pane shows the URLs that fit whole and counts the rest, on the one title
            // row, and the filters keep their corner.
            for (width, expected) in [
                (
                    140,
                    "┌Logs · http://app.tui.localhost:8080 · http://api.tui.localhost:8080 · +1 more ─",
                ),
                (110, "┌Logs · http://app.tui.localhost:8080 · +2 more ─"),
                (100, "┌Logs · +3 more ─"),
                (70, "┌Logs─"),
            ] {
                let (_, _, title) = render(&mut app, width);
                assert!(title.starts_with(expected), "{width} columns: {title}");
                assert!(
                    title.trim_end().ends_with("─ level ≥ WARN ┐"),
                    "{width} columns: {title}"
                );
            }

            // A service without routes keeps the plain title.
            app.state.selected_service = 1;
            let (_, _, title) = render(&mut app, 200);
            assert!(title.starts_with("┌Logs─"), "{title}");

            // A routing problem wakes the event loop and gets a header row of its own.
            let wake = app
                .summary_changes
                .clone()
                .ok_or_else(|| eyre::eyre!("a local session with routes has no summary feed"))?;
            session.set_routing(micromux::RoutingStatus::Unavailable {
                reason: "the proxy stopped answering".to_string(),
            });
            assert!(wake.has_changed()?);
            let (rows, header, _) = render(&mut app, 200);
            assert_eq!(header, 2, "{rows:#?}");
            assert!(
                rows[1].contains("routing unavailable: the proxy stopped answering"),
                "{rows:#?}"
            );
            Ok(())
        }

        /// A click on a URL in the Logs title opens that URL, since mouse capture keeps the
        /// terminal from opening it.
        #[tokio::test]
        async fn clicking_a_url_in_the_logs_title_opens_it() -> eyre::Result<()> {
            let directory = tempfile::tempdir()?;
            let yaml = indoc! {r#"
                version: 1
                session: { name: tui }
                proxy: { port: 8080 }
                routes:
                  - host: app
                    port: 9997
                  - host: app
                    path: /api/
                    port: 9998
                services:
                  web:
                    command: ["sleep", "30"]
                    ports: [9997, 9998]
            "#};
            let (mut app, _session, _) = routed_app(directory.path(), yaml)?;
            let screen = screen_text(&mut app, 200, 24);
            let (y, row) = screen
                .lines()
                .enumerate()
                .find(|(_, row)| row.contains("┌Logs"))
                .ok_or_else(|| eyre::eyre!("no Logs title:\n{screen}"))?;
            let column = |needle: &str| -> eyre::Result<u16> {
                let byte = row
                    .find(needle)
                    .ok_or_else(|| eyre::eyre!("{needle} is not in {row}"))?;
                Ok(u16::try_from(
                    row.get(..byte).unwrap_or_default().chars().count(),
                )?)
            };
            let y = u16::try_from(y)?;
            let click = |app: &mut App, x: u16| {
                for kind in [
                    crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                    crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left),
                ] {
                    app.handle_mouse(
                        crossterm::event::MouseEvent {
                            kind,
                            column: x,
                            row: y,
                            modifiers: KeyModifiers::NONE,
                        },
                        std::time::Instant::now(),
                    );
                }
            };

            // Both ends of the second URL open it, path included, and the separator between the
            // URLs opens nothing.
            let api = "http://app.tui.localhost:8080/api/";
            let start = column(api)?;
            let width = u16::try_from(api.len())?;
            click(&mut app, start);
            click(&mut app, start + width - 1);
            click(&mut app, start - 2);
            assert_eq!(opened(&mut app), vec![api.to_string(), api.to_string()]);
            click(&mut app, column("http://app.tui.localhost:8080")?);
            assert_eq!(
                opened(&mut app),
                vec!["http://app.tui.localhost:8080".to_string()]
            );

            // Without navigation, a click on a URL opens nothing.
            app.mouse = crate::mouse::MouseState::new(crate::MouseOptions {
                navigate: false,
                ..crate::MouseOptions::default()
            });
            click(&mut app, start);
            assert_eq!(app.effects, Vec::new());
            Ok(())
        }

        /// An app over a service with two URLs in its Logs title and three lines of its own logs,
        /// drawn 200 × 24, with where its URLs and its second line start.
        struct TitleUrls {
            app: App,
            api: Position,
            api_end: Position,
            root: Position,
            line: Position,
            #[expect(dead_code, reason = "keeps the session's registry and cache directory")]
            directory: tempfile::TempDir,
        }

        const API_URL: &str = "http://app.tui.localhost:8080/api/";
        const ROOT_URL: &str = "http://app.tui.localhost:8080";

        impl TitleUrls {
            fn new() -> eyre::Result<Self> {
                let directory = tempfile::tempdir()?;
                let yaml = indoc! {r#"
                    version: 1
                    session: { name: tui }
                    proxy: { port: 8080 }
                    routes:
                      - host: app
                        port: 9997
                      - host: app
                        path: /api/
                        port: 9998
                    services:
                      web:
                        command: ["sleep", "30"]
                        ports: [9997, 9998]
                      worker:
                        command: ["sleep", "30"]
                "#};
                let (mut app, _session, _) = routed_app(directory.path(), yaml)?;
                // The lines go in after the first frame read the model's empty log, which would
                // otherwise replace them.
                screen_text(&mut app, 200, 24);
                let service = app
                    .state
                    .current_service_mut()
                    .ok_or_else(|| eyre::eyre!("no service is selected"))?;
                service.logs_dirty = false;
                service.cached_lines = (1..=3)
                    .map(|seq| crate::state::CachedLine {
                        seq,
                        timestamp_unix_ms: 0,
                        text: format!("line {seq}"),
                    })
                    .collect();
                service.text_dirty = true;
                let screen = screen_text(&mut app, 200, 24);
                let api = find_on(&screen, API_URL)?;
                Ok(Self {
                    api,
                    api_end: Position::new(api.x + u16::try_from(API_URL.len())? - 1, api.y),
                    root: find_on(&screen, ROOT_URL)?,
                    line: find_on(&screen, "line 2")?,
                    app,
                    directory,
                })
            }

            /// Selects "line 2" with a drag, as a user does.
            fn select_line(&mut self) {
                let end = Position::new(self.line.x + 5, self.line.y);
                send(
                    &mut self.app,
                    MouseEventKind::Down(MouseButton::Left),
                    self.line,
                );
                send(&mut self.app, MouseEventKind::Drag(MouseButton::Left), end);
                send(&mut self.app, MouseEventKind::Up(MouseButton::Left), end);
            }
        }

        /// The cell where `needle` starts on `screen`.
        fn find_on(screen: &str, needle: &str) -> eyre::Result<Position> {
            for (y, row) in screen.lines().enumerate() {
                if let Some(byte) = row.find(needle) {
                    let x = row.get(..byte).unwrap_or_default().chars().count();
                    return Ok(Position::new(u16::try_from(x)?, u16::try_from(y)?));
                }
            }
            eyre::bail!("{needle} is not on screen:\n{screen}")
        }

        fn send(app: &mut App, kind: MouseEventKind, at: Position) {
            app.handle_mouse(
                MouseEvent {
                    kind,
                    column: at.x,
                    row: at.y,
                    modifiers: KeyModifiers::NONE,
                },
                std::time::Instant::now(),
            );
        }

        /// Presses the right button at `from` and releases it at `to`.
        fn right_click(app: &mut App, from: Position, to: Position) {
            send(app, MouseEventKind::Down(MouseButton::Right), from);
            send(app, MouseEventKind::Up(MouseButton::Right), to);
        }

        /// The URLs the effects so far open, which it leaves out of the effects; the others stay.
        fn opened(app: &mut App) -> Vec<String> {
            let mut urls = Vec::new();
            app.effects.retain(|effect| match effect {
                Effect::OpenUrl { url, .. } => {
                    urls.push(url.clone());
                    false
                }
                _ => true,
            });
            urls
        }

        fn copied(app: &mut App) -> Vec<String> {
            std::mem::take(&mut app.effects)
                .into_iter()
                .filter_map(|effect| match effect {
                    Effect::Copy(text) => Some(text),
                    _ => None,
                })
                .collect()
        }

        /// A right-click on a URL in the Logs title copies that URL, the one way to reach it over
        /// ssh, where no browser opens, and leaves the selection, the focus and the selected
        /// service as they are; between the URLs it copies the selection, as anywhere else.
        #[tokio::test]
        async fn right_clicking_a_url_in_the_logs_title_copies_it() -> eyre::Result<()> {
            let mut urls = TitleUrls::new()?;
            urls.select_line();
            let app = &mut urls.app;
            app.focus = Focus::Services;
            let selection = app.mouse.selection.clone();
            assert!(selection.is_some());

            // Either end of a URL copies that URL rather than the selection, path included.
            for at in [urls.api, urls.api_end] {
                right_click(app, at, at);
                assert_eq!(copied(app), vec![API_URL.to_string()], "{at:?}");
                assert_eq!(
                    app.mouse.notice,
                    Some(crate::mouse::Notice::Copied {
                        text: "copied the URL".to_string(),
                        url: true,
                    })
                );
            }
            right_click(app, urls.root, urls.root);
            assert_eq!(copied(app), vec![ROOT_URL.to_string()]);
            assert_eq!(app.mouse.selection, selection);
            assert_eq!(app.focus, Focus::Services);
            assert_eq!(app.state.selected_service, 0);

            let separator = Position::new(urls.api.x - 2, urls.api.y);
            right_click(app, separator, separator);
            assert_eq!(copied(app), vec!["line 2".to_string()]);
            Ok(())
        }

        /// Only a press and a release on the same URL copy it: a press on a URL that comes up
        /// elsewhere copies nothing, not even the selection, and a press elsewhere that comes up
        /// on a URL copies the selection, never the URL.
        #[tokio::test]
        async fn a_right_click_copies_a_url_only_pressed_and_released_on() -> eyre::Result<()> {
            let mut urls = TitleUrls::new()?;
            urls.select_line();
            let app = &mut urls.app;

            right_click(app, urls.api, urls.root);
            right_click(app, urls.api, urls.line);
            assert_eq!(copied(app), Vec::<String>::new());
            right_click(app, urls.line, urls.api);
            assert_eq!(copied(app), vec!["line 2".to_string()]);

            // Sliding off the URL cancels the copy, even when the pointer comes back to it, while
            // sliding along it does not.
            send(app, MouseEventKind::Down(MouseButton::Right), urls.api);
            send(app, MouseEventKind::Drag(MouseButton::Right), urls.line);
            send(app, MouseEventKind::Drag(MouseButton::Right), urls.api);
            send(app, MouseEventKind::Up(MouseButton::Right), urls.api);
            assert_eq!(copied(app), Vec::<String>::new());
            send(app, MouseEventKind::Down(MouseButton::Right), urls.api);
            send(app, MouseEventKind::Drag(MouseButton::Right), urls.api_end);
            send(app, MouseEventKind::Up(MouseButton::Right), urls.api_end);
            assert_eq!(copied(app), vec![API_URL.to_string()]);

            // The release copies, so a press alone does nothing yet.
            send(app, MouseEventKind::Down(MouseButton::Right), urls.api);
            assert_eq!(copied(app), Vec::<String>::new());
            // A URL that a new frame drew in the same cells between the press and the release is
            // not the one pressed, so it is not copied either.
            for link in &mut app.hits.links {
                link.url = format!("{}other/", link.url);
            }
            send(app, MouseEventKind::Up(MouseButton::Right), urls.api);
            assert_eq!(copied(app), Vec::<String>::new());
            Ok(())
        }

        /// Copying a URL is part of selecting: it needs `select`, and not `navigate`, which only
        /// opens URLs.
        #[tokio::test]
        async fn copying_a_url_needs_select_but_not_navigate() -> eyre::Result<()> {
            let mut urls = TitleUrls::new()?;
            let api = urls.api;
            let app = &mut urls.app;
            let right_then_left_click = |app: &mut App| {
                right_click(app, api, api);
                send(app, MouseEventKind::Down(MouseButton::Left), api);
                send(app, MouseEventKind::Up(MouseButton::Left), api);
                (opened(app), copied(app))
            };

            app.mouse = crate::mouse::MouseState::new(crate::MouseOptions {
                select: false,
                ..crate::MouseOptions::default()
            });
            assert!(app.mouse.capture);
            assert_eq!(
                right_then_left_click(app),
                (vec![API_URL.to_string()], vec![])
            );

            app.mouse = crate::mouse::MouseState::new(crate::MouseOptions {
                navigate: false,
                ..crate::MouseOptions::default()
            });
            assert_eq!(
                right_then_left_click(app),
                (vec![], vec![API_URL.to_string()])
            );
            Ok(())
        }

        /// Under a popup a right-click copies the popup's selection, even on a URL the popup
        /// leaves uncovered, and in PTY input mode it copies nothing, nor leaves a press on a URL
        /// for a later right-click to copy.
        #[tokio::test]
        async fn a_popup_or_pty_input_mode_keeps_a_right_click_off_urls() -> eyre::Result<()> {
            let mut urls = TitleUrls::new()?;
            let api = urls.api;
            let app = &mut urls.app;

            app.handle_key_press(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
            let popup = screen_text(app, 200, 24);
            assert!(
                popup
                    .lines()
                    .nth(usize::from(api.y))
                    .is_some_and(|row| row.contains(API_URL)),
                "the help popup covers the URL:\n{popup}"
            );
            let word = find_on(&popup, "Follow tail")?;
            let end = Position::new(word.x + 5, word.y);
            send(app, MouseEventKind::Down(MouseButton::Left), word);
            send(app, MouseEventKind::Drag(MouseButton::Left), end);
            send(app, MouseEventKind::Up(MouseButton::Left), end);
            right_click(app, api, api);
            assert_eq!(copied(app), vec!["Follow".to_string()]);
            assert!(app.show_help);

            app.show_help = false;
            screen_text(app, 200, 24);
            app.enter_pty_input_mode();
            right_click(app, api, api);
            assert_eq!(copied(app), Vec::<String>::new());

            // A press on the URL whose release, or a later press, comes before the frame shows a
            // popup the key opened, is dropped there and leaves nothing for a lone release on the
            // URL, as from a gesture whose own press was lost, to copy.
            app.leave_pty_input_mode();
            let help = KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE);
            for kind in [
                MouseEventKind::Up(MouseButton::Right),
                MouseEventKind::Down(MouseButton::Right),
            ] {
                send(app, MouseEventKind::Down(MouseButton::Right), api);
                app.handle_key_press(help);
                send(app, kind, api);
                app.handle_key_press(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                send(app, MouseEventKind::Up(MouseButton::Right), api);
                assert_eq!(copied(app), Vec::<String>::new(), "{kind:?}");
            }

            // A press on the URL before PTY input mode, released in it, and a press in the logs
            // in it, released on the URL after it, never join into a click on the URL.
            send(app, MouseEventKind::Down(MouseButton::Right), api);
            app.enter_pty_input_mode();
            send(app, MouseEventKind::Up(MouseButton::Right), api);
            send(app, MouseEventKind::Down(MouseButton::Right), urls.line);
            app.leave_pty_input_mode();
            send(app, MouseEventKind::Up(MouseButton::Right), api);
            assert_eq!(copied(app), Vec::<String>::new());
            Ok(())
        }

        /// A click acts on a URL only where the frame shows it: in a Logs pane one row tall, the
        /// title keeps its row from the bottom border, and a terminal so short that the header
        /// and footer take every row draws no Logs title, so no URL waits in the footer's cells.
        #[tokio::test]
        async fn urls_are_clicked_only_where_the_title_shows_them() -> eyre::Result<()> {
            let mut urls = TitleUrls::new()?;
            let app = &mut urls.app;
            let chrome = app.chrome_rows(200);
            for height in [24, chrome.header + chrome.footer + 1] {
                let screen = screen_text(app, 200, height);
                assert_eq!(
                    app.hits.links.len(),
                    2,
                    "{height} rows: {:?}",
                    app.hits.links
                );
                for link in &app.hits.links {
                    let shown = screen
                        .lines()
                        .nth(usize::from(link.area.y))
                        .unwrap_or_default()
                        .chars()
                        .skip(usize::from(link.area.x))
                        .take(usize::from(link.area.width))
                        .collect::<String>();
                    assert_eq!(shown, link.url, "{height} rows:\n{screen}");
                }
            }
            screen_text(app, 200, chrome.header + chrome.footer);
            assert_eq!(app.hits.links, Vec::new());
            Ok(())
        }

        /// Presses and releases the left button at `at`.
        fn left_click(app: &mut App, at: Position) {
            send(app, MouseEventKind::Down(MouseButton::Left), at);
            send(app, MouseEventKind::Up(MouseButton::Left), at);
        }

        /// Drags with the left button from `from` to `to`.
        fn left_drag(app: &mut App, from: Position, to: Position) {
            send(app, MouseEventKind::Down(MouseButton::Left), from);
            send(app, MouseEventKind::Drag(MouseButton::Left), to);
            send(app, MouseEventKind::Up(MouseButton::Left), to);
        }

        /// Copies the selection with `y` and returns what was copied.
        fn yank(app: &mut App) -> Vec<String> {
            app.handle_key_press(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
            copied(app)
        }

        /// A URL in the Logs title selects like a pane's text: a drag along it selects it,
        /// highlighted, and opens nothing, and a selection never leaves the URL, so the title
        /// around it never selects.
        #[tokio::test]
        async fn dragging_along_a_url_selects_it_without_opening_it() -> eyre::Result<()> {
            let mut urls = TitleUrls::new()?;
            let app = &mut urls.app;
            left_drag(app, urls.api, urls.api_end);
            assert_eq!(app.effects, Vec::new());
            let area = ratatui::layout::Rect::new(0, 0, 200, 24);
            let mut frame = ratatui::buffer::Buffer::empty(area);
            ratatui::widgets::Widget::render(&mut *app, area, &mut frame);
            let reversed = |x: u16| {
                frame
                    .cell((x, urls.api.y))
                    .is_some_and(|cell| cell.modifier.contains(ratatui::style::Modifier::REVERSED))
            };
            assert!((urls.api.x..=urls.api_end.x).all(reversed));
            assert!(!reversed(urls.api.x - 1) && !reversed(urls.api_end.x + 1));
            assert_eq!(yank(app), vec![API_URL.to_string()]);

            // From the URL's end back over the separator and the URL before it.
            left_drag(app, urls.api_end, urls.root);
            assert_eq!(app.effects, Vec::new());
            assert_eq!(yank(app), vec![API_URL.to_string()]);

            // A triple click on the pane's name selects nothing, and one on a URL the URL.
            let logs = find_on(&screen_text(app, 200, 24), "┌Logs")?;
            for _ in 0..3 {
                left_click(app, Position::new(logs.x + 1, logs.y));
            }
            assert_eq!(yank(app), Vec::<String>::new());
            app.mouse = crate::mouse::MouseState::new(crate::MouseOptions {
                navigate: false,
                ..crate::MouseOptions::default()
            });
            for _ in 0..3 {
                left_click(app, urls.api);
            }
            assert_eq!(yank(app), vec![API_URL.to_string()]);
            Ok(())
        }

        /// A click on a URL opens it, and a quick second click selects it without opening it
        /// again.
        #[tokio::test]
        async fn a_double_click_on_a_url_opens_it_once_and_selects_it() -> eyre::Result<()> {
            let mut urls = TitleUrls::new()?;
            let app = &mut urls.app;
            left_click(app, urls.api);
            // The click leaves no caret behind, so `Esc` would go on to clear a search.
            assert_eq!(app.mouse.selection, None);
            left_click(app, urls.api);
            assert_eq!(opened(app), vec![API_URL.to_string()]);
            assert_eq!(yank(app), vec![API_URL.to_string()]);

            // A drag along the URL opened nothing, so a quick click after it, on the cell it
            // started from, still opens the URL.
            left_drag(app, urls.root, Position::new(urls.root.x + 5, urls.root.y));
            left_click(app, urls.root);
            assert_eq!(opened(app), vec![ROOT_URL.to_string()]);
            Ok(())
        }

        /// Without navigation, a click or a drag on a URL selects and never opens; without
        /// selecting, a click on a URL opens and a drag selects nothing.
        #[tokio::test]
        async fn the_title_selects_as_its_mouse_parts_allow() -> eyre::Result<()> {
            let mut urls = TitleUrls::new()?;
            let app = &mut urls.app;
            app.mouse = crate::mouse::MouseState::new(crate::MouseOptions {
                navigate: false,
                ..crate::MouseOptions::default()
            });
            left_click(app, urls.api);
            left_drag(app, urls.api, urls.api_end);
            assert_eq!(copied(app), Vec::<String>::new());
            assert_eq!(app.effects, Vec::new());
            assert_eq!(yank(app), vec![API_URL.to_string()]);

            app.mouse = crate::mouse::MouseState::new(crate::MouseOptions {
                select: false,
                ..crate::MouseOptions::default()
            });
            left_drag(app, urls.root, Position::new(urls.root.x + 5, urls.root.y));
            assert_eq!(opened(app), vec![ROOT_URL.to_string()]);
            assert_eq!(yank(app), Vec::<String>::new());
            Ok(())
        }

        /// A selection in a URL holds only while the title shows that URL in its place:
        /// selecting another service, or a narrower pane that no longer fits it, takes it away,
        /// while a URL that still fits keeps its selection.
        #[tokio::test]
        async fn a_url_selection_goes_with_its_url() -> eyre::Result<()> {
            let mut urls = TitleUrls::new()?;
            let app = &mut urls.app;
            left_drag(app, urls.api, urls.api_end);
            screen_text(app, 200, 24);
            assert!(app.has_selection());
            app.state.selected_service = 1;
            screen_text(app, 200, 24);
            assert!(!app.has_selection());

            app.state.selected_service = 0;
            screen_text(app, 200, 24);
            left_drag(app, urls.root, Position::new(urls.root.x + 3, urls.root.y));
            let narrow = screen_text(app, 100, 24);
            assert!(narrow.contains("+1 more"), "{narrow}");
            assert_eq!(yank(app), vec!["http".to_string()]);

            screen_text(app, 200, 24);
            left_drag(app, urls.api, urls.api_end);
            screen_text(app, 100, 24);
            assert!(!app.has_selection());
            assert_eq!(yank(app), Vec::<String>::new());
            Ok(())
        }

        /// A frame that takes away a selection whose URL went draws again, so the footer stops
        /// offering to copy it.
        #[tokio::test]
        async fn the_footer_drops_the_copy_hint_with_the_selection() -> eyre::Result<()> {
            let mut urls = TitleUrls::new()?;
            let app = &mut urls.app;
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(200, 24))?;
            app.draw(&mut terminal)?;
            left_drag(app, urls.api, urls.api_end);
            let footer = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| {
                let buffer = terminal.backend().buffer();
                (0..buffer.area.width)
                    .filter_map(|x| buffer.cell((x, buffer.area.height - 1)))
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
            };
            app.draw(&mut terminal)?;
            assert!(
                footer(&terminal).contains("y Copy"),
                "{}",
                footer(&terminal)
            );

            app.handle_key_press(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE));
            app.draw(&mut terminal)?;
            assert_eq!(app.mouse.selection, None);
            assert!(
                !footer(&terminal).contains("y Copy"),
                "{}",
                footer(&terminal)
            );
            Ok(())
        }

        /// The one URL the effects so far open and the notice count of its click, which it takes
        /// out of them.
        fn open_stamp(app: &mut App) -> eyre::Result<(String, u64)> {
            let opens = std::mem::take(&mut app.effects)
                .into_iter()
                .filter_map(|effect| match effect {
                    Effect::OpenUrl {
                        url,
                        notice_changes,
                    } => Some((url, notice_changes)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            match opens.as_slice() {
                [open] => Ok(open.clone()),
                _ => eyre::bail!("the click opened {} URLs", opens.len()),
            }
        }

        /// A URL that cannot be opened says so only while nothing after its click, handled in the
        /// same batch of input before the opener runs, changed what the footer says: not a copy,
        /// not a key, and not a click that opened another URL.
        #[tokio::test]
        async fn a_failed_open_never_overwrites_a_later_notice() -> eyre::Result<()> {
            let mut urls = TitleUrls::new()?;
            let app = &mut urls.app;
            let now = std::time::Instant::now();
            let failed = |url: &str| {
                Some(crate::mouse::Notice::Failed {
                    url: url.to_string(),
                })
            };

            // Alone, a failure says so.
            left_click(app, urls.api);
            let (stamp_url, stamp) = open_stamp(app)?;
            app.url_open_failed(&stamp_url, stamp, now);
            assert_eq!(app.mouse.notice, failed(&stamp_url));

            // A right-click that copied a URL after the click.
            left_click(app, urls.root);
            let (stamp_url, stamp) = open_stamp(app)?;
            right_click(app, urls.api, urls.api);
            app.url_open_failed(&stamp_url, stamp, now);
            assert_eq!(
                app.mouse.notice,
                Some(crate::mouse::Notice::Copied {
                    text: "copied the URL".to_string(),
                    url: true,
                })
            );

            // A click that opened another URL after it, which opened.
            left_click(app, urls.api);
            let (first_url, first) = open_stamp(app)?;
            left_click(app, urls.root);
            let (second_url, second) = open_stamp(app)?;
            app.url_open_failed(&first_url, first, now);
            assert_eq!(app.mouse.notice, None);
            app.url_open_failed(&second_url, second, now);
            assert_eq!(app.mouse.notice, failed(&second_url));

            // A key that cleared the notice after the click, here opening the help.
            left_click(app, urls.api);
            let (stamp_url, stamp) = open_stamp(app)?;
            app.handle_crossterm_event(&crossterm::event::Event::Key(KeyEvent::new(
                KeyCode::Char('?'),
                KeyModifiers::NONE,
            )));
            app.url_open_failed(&stamp_url, stamp, now);
            assert_eq!(app.mouse.notice, None);
            Ok(())
        }

        /// A URL's copy whose notice has no room beside the footer's hints says that a URL was
        /// copied, never that the highlighted selection was, and keeps the footer's rows.
        #[tokio::test]
        async fn a_url_copy_never_reads_as_the_selections() -> eyre::Result<()> {
            // Right-clicks copy whether or not clicks navigate, and so does the note.
            for navigate in [true, false] {
                let mut urls = TitleUrls::new()?;
                urls.app.mouse = crate::mouse::MouseState::new(crate::MouseOptions {
                    navigate,
                    ..crate::MouseOptions::default()
                });
                urls.select_line();
                let app = &mut urls.app;
                let before = (60..=200)
                    .map(|width| app.chrome_rows(width))
                    .collect::<Vec<_>>();
                right_click(app, urls.api, urls.api);
                assert_eq!(copied(app), vec![API_URL.to_string()]);
                let footer = |app: &mut App, width: u16| {
                    let screen = screen_text(app, width, 24);
                    let rows = usize::from(app.chrome_rows(width).footer);
                    let lines = screen.lines().collect::<Vec<_>>();
                    lines[lines.len() - rows..].join("\n")
                };
                let full = (60..=200)
                    .rev()
                    .find(|&width| !footer(app, width).contains("copied the URL"))
                    .ok_or_else(|| eyre::eyre!("every footer has room for the notice"))?;
                let shown = footer(app, full);
                assert!(shown.contains("URL copied"), "{full} columns:\n{shown}");
                assert!(!shown.contains("y Copied"), "{shown}");
                let after = (60..=200)
                    .map(|width| app.chrome_rows(width))
                    .collect::<Vec<_>>();
                assert_eq!(after, before);

                // A press on the note ends it and nothing else: the selection the URL's copy kept
                // stays, and the `y Copy` hint the next frame puts back there does not act on the
                // release.
                let note = {
                    let screen = screen_text(app, full, 24);
                    find_on(&screen, "URL copied")?
                };
                send(app, MouseEventKind::Down(MouseButton::Left), note);
                assert_eq!(app.mouse.notice, None);
                let shown = footer(app, full);
                assert!(shown.contains("y Copy"), "{shown}");
                send(app, MouseEventKind::Up(MouseButton::Left), note);
                assert_eq!(app.effects, Vec::new());
                assert_eq!(yank(app), vec!["line 2".to_string()]);
            }
            Ok(())
        }

        /// An opener that exits with an error says so only soon after its click, as a browser that
        /// some openers wait for can quit with an error long after it opened the URL, and offers
        /// the right-click that copies the URL only while the Logs title still shows it.
        #[tokio::test]
        async fn an_opener_error_says_so_only_soon_and_while_its_url_shows() -> eyre::Result<()> {
            let mut urls = TitleUrls::new()?;
            let app = &mut urls.app;
            let now = std::time::Instant::now();
            // Each click lands on another URL than the last, so none is a double click.
            let failure = |app: &mut App, on: Position, at: std::time::Instant| {
                left_click(app, on);
                let (url, notice_changes) = open_stamp(app)?;
                eyre::Ok(OpenerFailure {
                    url,
                    notice_changes,
                    clicked_at: at,
                })
            };

            // Long after its click, an error says nothing.
            let late = failure(app, urls.api, now)?;
            app.opener_failed(&late, now + OPENER_FAILURE_WINDOW + Duration::from_secs(1));
            assert_eq!(app.mouse.notice, None);

            let soon = failure(app, urls.root, now)?;
            app.opener_failed(&soon, now + Duration::from_secs(2));
            let screen = screen_text(app, 200, 24);
            assert!(
                screen.contains("cannot open the URL; right-click it to copy it"),
                "{screen}"
            );

            // The note says what the frame it is drawn in allows: once another service is selected,
            // as when the clicked one went after the failure arrived, its URL is gone.
            let gone = failure(app, urls.api, now)?;
            app.opener_failed(&gone, now);
            app.state.selected_service = 1;
            let screen = screen_text(app, 200, 24);
            assert!(screen.contains("cannot open the URL"), "{screen}");
            assert!(!screen.contains("right-click it"), "{screen}");
            Ok(())
        }

        /// A URL shows in the Logs title only where it fits whole beside the search status as the
        /// terminal draws it, a cell wider than its unicode width for each halfwidth katakana sound
        /// mark, so at every width each URL a click opens is drawn where the click lands.
        #[tokio::test]
        async fn title_urls_fit_beside_a_status_with_sound_marks() -> eyre::Result<()> {
            let mut urls = TitleUrls::new()?;
            let app = &mut urls.app;
            let key = |app: &mut App, code: KeyCode| {
                app.handle_crossterm_event(&crossterm::event::Event::Key(KeyEvent::new(
                    code,
                    KeyModifiers::NONE,
                )));
            };
            key(app, KeyCode::Char('/'));
            for ch in "ｶﾞｶﾞｶﾞｶﾞ".chars() {
                key(app, KeyCode::Char(ch));
            }
            key(app, KeyCode::Enter);

            let mut shown = 0;
            for width in 60..=160 {
                let area = ratatui::layout::Rect::new(0, 0, width, 24);
                let mut buffer = ratatui::buffer::Buffer::empty(area);
                ratatui::widgets::Widget::render(&mut *app, area, &mut buffer);
                for link in &app.hits.links {
                    let drawn = (link.area.left()..link.area.right())
                        .filter_map(|x| buffer.cell((x, link.area.y)))
                        .map(ratatui::buffer::Cell::symbol)
                        .collect::<String>();
                    assert_eq!(drawn, link.url, "{width} columns");
                    shown += 1;
                }
                let title = (0..width)
                    .filter_map(|x| buffer.cell((x, app.hits.logs.y)))
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>();
                assert!(title.contains("no match ┐"), "{width} columns: {title}");
            }
            assert!(shown > 0);
            Ok(())
        }

        /// A failed opener wakes the event loop by itself, with no input after it, so an idle TUI
        /// still says so.
        #[tokio::test]
        async fn a_failed_opener_wakes_the_event_loop() -> eyre::Result<()> {
            let mut urls = TitleUrls::new()?;
            let app = &mut urls.app;
            let failure = OpenerFailure {
                url: API_URL.to_string(),
                notice_changes: 0,
                clicked_at: std::time::Instant::now(),
            };
            app.opener_failures.send(failure.clone())?;
            let mut continued = platform::Continued::watch();
            // The session's own start may wake the loop first.
            let woke = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Some(Wake::OpenerFailed(failed)) = app.next_wake(&mut continued).await {
                        break failed;
                    }
                }
            })
            .await?;
            assert_eq!(woke, failure);
            Ok(())
        }

        /// A right-click on a URL copies the whole URL even while part of the title is selected,
        /// and leaves that selection for `y` to copy.
        #[tokio::test]
        async fn a_right_click_on_a_url_keeps_a_title_selection() -> eyre::Result<()> {
            let mut urls = TitleUrls::new()?;
            let app = &mut urls.app;
            left_drag(app, urls.api, Position::new(urls.api.x + 10, urls.api.y));
            assert_eq!(app.effects, Vec::new());
            right_click(app, urls.root, urls.root);
            assert_eq!(copied(app), vec![ROOT_URL.to_string()]);
            assert_eq!(yank(app), vec!["http://app.".to_string()]);
            Ok(())
        }

        /// The whole screen the app renders at `width` × `height`, one line per row.
        fn screen_text(app: &mut App, width: u16, height: u16) -> String {
            let area = ratatui::layout::Rect::new(0, 0, width, height);
            let mut buffer = ratatui::buffer::Buffer::empty(area);
            ratatui::widgets::Widget::render(&mut *app, area, &mut buffer);
            (0..height)
                .map(|y| {
                    (0..width)
                        .filter_map(|x| buffer.cell((x, y)).map(ratatui::buffer::Cell::symbol))
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// `i` describes the selected service and `u` the session in a popup; each key toggles its
        /// own view and switches from the other, and Esc closes either.
        #[tokio::test]
        async fn info_popups_describe_the_service_and_the_session() -> eyre::Result<()> {
            let directory = tempfile::tempdir()?;
            let yaml = indoc! {r#"
                version: 1
                session: { name: tui }
                proxy: { port: 8080 }
                allocate_ports: [WEB_PORT]
                routes:
                  - host: app
                    port: ${WEB_PORT}
                  - host: app
                    path: /auth/v1/
                    strip_prefix: true
                    port: 9998
                  - host: "*"
                    port: 9999
                services:
                  db:
                    command: ["sleep", "30"]
                  web:
                    command: ["sh", "-c", "sleep 30"]
                    ports: ["${WEB_PORT}"]
                    depends_on: [db]
                    restart: on-failure
            "#};
            let (mut app, _session, ports) = routed_app(directory.path(), yaml)?;
            let port = ports["WEB_PORT"];
            let press = |app: &mut App, code| {
                app.handle_key_press(KeyEvent {
                    code,
                    modifiers: KeyModifiers::NONE,
                    kind: KeyEventKind::Press,
                    state: KeyEventState::NONE,
                });
            };
            let screen = |app: &mut App| screen_text(app, 160, 40);
            app.state.selected_service = 1;

            let closed = screen(&mut app);
            assert!(closed.contains("i Info"), "{closed}");
            assert!(closed.contains("u URLs"), "{closed}");

            press(&mut app, KeyCode::Char('i'));
            let service = screen(&mut app);
            for expected in [
                "Service · web",
                "URLs        http://app.tui.localhost:8080",
                &format!("Ports       :{port}  WEB_PORT"),
                "Command     sh -c 'sleep 30'",
                "Depends on  db (started)",
                "Restart     on failure",
                "i or Esc closes",
            ] {
                assert!(service.contains(expected), "{expected}\n{service}");
            }

            press(&mut app, KeyCode::Char('u'));
            let session = screen(&mut app);
            for expected in [
                "Session · tui",
                "Session     tui.localhost",
                "Proxy       :8080",
                // A path route keeps its trailing `/`, which is part of its prefix.
                &format!(
                    "Routes      {:<38}  → :{port}  web",
                    "http://app.tui.localhost:8080"
                ),
                "http://app.tui.localhost:8080/auth/v1/  → :9998  no single service  (prefix stripped)",
                &format!(
                    "{:<38}  → :9999  no single service  (catch-all)",
                    "http://tui.localhost:8080"
                ),
                &format!("Ports       :{port}  WEB_PORT"),
            ] {
                assert!(session.contains(expected), "{expected}\n{session}");
            }

            // The popup swallows other keys, its own key closes it, and Esc closes the other one.
            press(&mut app, KeyCode::Char('k'));
            assert_eq!(app.state.selected_service, 1);
            press(&mut app, KeyCode::Char('u'));
            assert!(!screen(&mut app).contains("Session · tui"));
            press(&mut app, KeyCode::Char('i'));
            press(&mut app, KeyCode::Esc);
            assert!(!screen(&mut app).contains("Service · web"));

            // A popup taller than the terminal scrolls instead of cutting rows off.
            press(&mut app, KeyCode::Char('i'));
            let small = |app: &mut App| screen_text(app, 70, 14);
            let top = small(&mut app);
            assert!(top.contains("State"), "{top}");
            assert!(top.contains("↑/↓ scroll · 1-"), "{top}");
            assert!(!top.contains("Run "), "{top}");
            press(&mut app, KeyCode::Char('G'));
            let bottom = small(&mut app);
            assert!(bottom.contains("Run "), "{bottom}");
            assert!(!bottom.contains("State"), "{bottom}");
            press(&mut app, KeyCode::Esc);

            press(&mut app, KeyCode::Char('?'));
            let help = screen(&mut app);
            assert!(help.contains("Service details"), "{help}");
            assert!(help.contains("Session URLs and ports"), "{help}");
            Ok(())
        }
    }

    #[tokio::test]
    async fn terminal_eof_is_visible_without_stopping_the_session() -> eyre::Result<()> {
        let mut diagnostics = Vec::new();
        let parsed = micromux::from_str(
            "version: 1\nservices: {}\n",
            Path::new("."),
            0usize,
            None,
            &mut diagnostics,
        )?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let shutdown = micromux::CancellationToken::new();
        let (_runner, handles) = mux.start(shutdown.clone());
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal),
            shutdown.clone(),
            ViewOptions::default(),
        );

        // With no service to describe, `i` opens nothing that would swallow later keys.
        app.handle_key_press(KeyEvent {
            code: KeyCode::Char('i'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        assert!(app.info.is_none());

        app.handle_input_event(crate::event::Input::Closed);
        // Successful input clears transient notices; terminal EOF must remain visible through that
        // lifecycle.
        app.input_notice = None;

        assert!(app.running);
        assert!(!shutdown.is_cancelled());
        assert!(app.terminal_input_closed);

        let area = ratatui::layout::Rect::new(0, 0, 160, 24);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        (&mut app).render(area, &mut buffer);
        let text = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(text.contains(TERMINAL_INPUT_CLOSED_NOTICE));
        Ok(())
    }

    #[tokio::test]
    async fn healthcheck_text_is_parsed_once_until_the_model_marks_it_dirty() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["true"]
                healthcheck:
                  test: ["true"]
        "#};
        let mut diagnostics = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let (_runner, handles) = mux.start(micromux::CancellationToken::new());
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal.clone()),
            micromux::CancellationToken::new(),
            ViewOptions::default(),
        );
        app.side_pane = Some(SidePane::Health);
        let area = ratatui::layout::Rect::new(0, 0, 120, 30);
        let mut buffer = ratatui::buffer::Buffer::empty(area);

        (&mut app).render(area, &mut buffer);
        let cached_pointer = app
            .state
            .current_service()
            .and_then(|service| service.health_pane.text.lines.first())
            .and_then(|line| line.spans.first())
            .map(|span| span.content.as_ptr())
            .ok_or_else(|| eyre::eyre!("healthcheck cache was empty"))?;
        (&mut app).render(area, &mut buffer);
        let reused_pointer = app
            .state
            .current_service()
            .and_then(|service| service.health_pane.text.lines.first())
            .and_then(|line| line.spans.first())
            .map(|span| span.content.as_ptr())
            .ok_or_else(|| eyre::eyre!("healthcheck cache was empty"))?;

        assert_eq!(reused_pointer, cached_pointer);
        assert!(
            app.state
                .current_service()
                .is_some_and(|service| !service.healthcheck_dirty)
        );
        Ok(())
    }

    /// Keys reach a service as the bytes a legacy terminal sends, whether micromux's terminal
    /// reported them in legacy form, through the kitty keyboard protocol or through tmux's
    /// modifyOtherKeys reports, which report some keys apart that legacy terminals send alike.
    #[test]
    fn keys_reach_a_service_as_a_legacy_terminal_sends_them() {
        const NONE: KeyModifiers = KeyModifiers::NONE;
        const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
        const CTRL: KeyModifiers = KeyModifiers::CONTROL;
        const ALT: KeyModifiers = KeyModifiers::ALT;
        const SUPER: KeyModifiers = KeyModifiers::SUPER;
        let cases: &[(KeyCode, KeyModifiers, &[u8])] = &[
            // Text.
            (KeyCode::Char('a'), NONE, b"a"),
            (KeyCode::Char('A'), SHIFT, b"A"),
            (KeyCode::Char('!'), SHIFT, b"!"),
            (KeyCode::Char('é'), NONE, "é".as_bytes()),
            (KeyCode::Char(' '), NONE, b" "),
            // Control characters, the first as the kitty protocol reports Ctrl+Shift+C, with
            // alternate keys reported and without.
            (KeyCode::Char('c'), CTRL, b"\x03"),
            (KeyCode::Char('c'), CTRL | SHIFT, b"\x03"),
            (KeyCode::Char('C'), CTRL, b"\x03"),
            (KeyCode::Char('z'), CTRL, b"\x1a"),
            (KeyCode::Char('h'), CTRL, b"\x08"),
            (KeyCode::Char('i'), CTRL, b"\t"),
            (KeyCode::Char('m'), CTRL, b"\r"),
            (KeyCode::Char(' '), CTRL, b"\x00"),
            (KeyCode::Char('@'), CTRL, b"\x00"),
            (KeyCode::Char('['), CTRL, b"\x1b"),
            (KeyCode::Char('\\'), CTRL, b"\x1c"),
            (KeyCode::Char(']'), CTRL, b"\x1d"),
            (KeyCode::Char('^'), CTRL, b"\x1e"),
            (KeyCode::Char('_'), CTRL, b"\x1f"),
            (KeyCode::Char('/'), CTRL, b"\x1f"),
            (KeyCode::Char('?'), CTRL, b"\x7f"),
            // Legacy terminals send Ctrl+4 to Ctrl+7 as these, which crossterm reads back as
            // digits.
            (KeyCode::Char('4'), CTRL, b"\x1c"),
            (KeyCode::Char('7'), CTRL, b"\x1f"),
            (KeyCode::Char('1'), CTRL, b"1"),
            // Alt prefixes Escape, with Ctrl too, and Shift picks the capital.
            (KeyCode::Char('x'), ALT, b"\x1bx"),
            (KeyCode::Char('a'), ALT | SHIFT, b"\x1bA"),
            (KeyCode::Char('>'), ALT, b"\x1b>"),
            // As tmux reports keys under modifyOtherKeys: Alt+Shift+C as a capital with Alt
            // alone, Ctrl+Shift+Z with both.
            (KeyCode::Char('C'), ALT, b"\x1bC"),
            (KeyCode::Char('z'), CTRL | SHIFT, b"\x1a"),
            (KeyCode::Char('c'), CTRL | ALT, b"\x1b\x03"),
            (KeyCode::Enter, ALT, b"\x1b\r"),
            (KeyCode::Backspace, ALT, b"\x1b\x7f"),
            // Super never reaches a legacy terminal's programs, so the key goes alone.
            (KeyCode::Char('c'), SUPER, b"c"),
            // Editing keys, with any modifier.
            (KeyCode::Esc, NONE, b"\x1b"),
            (KeyCode::Enter, NONE, b"\r"),
            (KeyCode::Enter, SHIFT, b"\r"),
            (KeyCode::Enter, CTRL, b"\r"),
            (KeyCode::Tab, NONE, b"\t"),
            (KeyCode::Tab, CTRL, b"\t"),
            (KeyCode::BackTab, SHIFT, b"\x1b[Z"),
            (KeyCode::Backspace, NONE, b"\x7f"),
            (KeyCode::Backspace, CTRL, b"\x08"),
            // Cursor and editing keys carry their modifiers as xterm's parameter.
            (KeyCode::Up, NONE, b"\x1b[A"),
            (KeyCode::Down, NONE, b"\x1b[B"),
            (KeyCode::Right, NONE, b"\x1b[C"),
            (KeyCode::Left, NONE, b"\x1b[D"),
            (KeyCode::Home, NONE, b"\x1b[H"),
            (KeyCode::End, NONE, b"\x1b[F"),
            (KeyCode::Up, SHIFT, b"\x1b[1;2A"),
            (KeyCode::Left, ALT, b"\x1b[1;3D"),
            (KeyCode::Right, CTRL, b"\x1b[1;5C"),
            (KeyCode::End, CTRL | SHIFT, b"\x1b[1;6F"),
            (KeyCode::Insert, NONE, b"\x1b[2~"),
            (KeyCode::Delete, NONE, b"\x1b[3~"),
            (KeyCode::PageUp, NONE, b"\x1b[5~"),
            (KeyCode::PageDown, NONE, b"\x1b[6~"),
            (KeyCode::Delete, CTRL, b"\x1b[3;5~"),
            (KeyCode::PageUp, SHIFT, b"\x1b[5;2~"),
            // Function keys.
            (KeyCode::F(1), NONE, b"\x1bOP"),
            (KeyCode::F(4), NONE, b"\x1bOS"),
            (KeyCode::F(1), SHIFT, b"\x1b[1;2P"),
            (KeyCode::F(5), NONE, b"\x1b[15~"),
            (KeyCode::F(6), NONE, b"\x1b[17~"),
            (KeyCode::F(10), NONE, b"\x1b[21~"),
            (KeyCode::F(11), NONE, b"\x1b[23~"),
            (KeyCode::F(12), NONE, b"\x1b[24~"),
            (KeyCode::F(5), CTRL, b"\x1b[15;5~"),
        ];
        for &(code, modifiers, bytes) in cases {
            assert_eq!(
                key_event_to_bytes(code, modifiers).as_deref(),
                Some(bytes),
                "{code:?} with {modifiers:?}"
            );
        }
        // Keys a legacy terminal has no bytes for send nothing.
        assert_eq!(key_event_to_bytes(KeyCode::F(13), NONE), None);
        assert_eq!(key_event_to_bytes(KeyCode::CapsLock, NONE), None);
    }

    /// Ctrl+Shift+C and Ctrl+Insert, as on Linux and Windows, and Super+C, as Cmd+C on macOS,
    /// copy; Ctrl-C and the chords near these never do.
    #[test]
    fn the_terminals_copy_shortcuts_are_copy_chords_and_ctrl_c_is_not() {
        let chord = |code, modifiers| {
            is_copy_chord(KeyEvent {
                code,
                modifiers,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            })
        };
        let ctrl_shift = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
        assert!(chord(KeyCode::Char('c'), ctrl_shift));
        // With alternate keys reported, the shifted key comes without Shift.
        assert!(chord(KeyCode::Char('C'), KeyModifiers::CONTROL));
        assert!(chord(KeyCode::Char('C'), ctrl_shift));
        assert!(chord(KeyCode::Insert, KeyModifiers::CONTROL));
        assert!(chord(KeyCode::Char('c'), KeyModifiers::SUPER));

        assert!(!chord(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!chord(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(!chord(KeyCode::Char('C'), KeyModifiers::SHIFT));
        assert!(!chord(KeyCode::Char('c'), KeyModifiers::ALT));
        assert!(!chord(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        ));
        assert!(!chord(KeyCode::Char('C'), KeyModifiers::SUPER));
        assert!(!chord(KeyCode::Insert, KeyModifiers::NONE));
        assert!(!chord(KeyCode::Insert, KeyModifiers::SHIFT));
    }

    #[tokio::test]
    async fn pty_input_waits_in_the_global_budget_when_the_scheduler_queue_is_full()
    -> eyre::Result<()> {
        let yaml = "version: 1\nservices:\n  svc:\n    command: [\"true\"]\n";
        let mut diagnostics: Vec<Diagnostic<usize>> = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!(err.to_string()))?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let shutdown = micromux::CancellationToken::new();
        let (runner, handles) = mux.start(shutdown.clone());
        assert_eq!(fill_terminal_queue(&handles.terminal)?, 64);
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal),
            shutdown.clone(),
            ViewOptions::default(),
        );

        app.enqueue_pty_input("svc".to_string(), b"first".to_vec());
        app.enqueue_pty_input("svc".to_string(), b"second".to_vec());
        assert_eq!(app.pending_pty_input.len(), 1);
        assert_eq!(
            app.pending_pty_input.front().map(PreparedPtyInput::data),
            Some(b"firstsecond".as_slice())
        );

        let runner = tokio::spawn(runner);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !app.pending_pty_input.is_empty() {
                app.flush_pending_pty_input();
                tokio::task::yield_now().await;
            }
        })
        .await?;
        shutdown.cancel();
        runner.await??;
        Ok(())
    }

    #[tokio::test]
    async fn bracketed_paste_is_inert_in_command_mode_and_atomic_in_input_mode() -> eyre::Result<()>
    {
        let yaml = "version: 1\nservices:\n  svc:\n    command: [\"true\"]\n";
        let mut diagnostics: Vec<Diagnostic<usize>> = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!(err.to_string()))?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let (_runner, handles) = mux.start(micromux::CancellationToken::new());
        assert_eq!(fill_terminal_queue(&handles.terminal)?, 64);
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal),
            micromux::CancellationToken::new(),
            ViewOptions::default(),
        );

        app.handle_crossterm_event(&crossterm::event::Event::Paste("Rq".to_string()));
        assert!(app.running);
        assert!(app.pending_pty_input.is_empty());

        app.pty_input_mode = true;
        let paste = "x".repeat(micromux::MAX_PTY_INPUT_BATCH_BYTES + 17);
        app.handle_crossterm_event(&crossterm::event::Event::Paste(paste.clone()));
        let prepared = app
            .pending_pty_input
            .front()
            .ok_or_else(|| eyre::eyre!("paste was not retained"))?;
        assert_eq!(prepared.service_id(), "svc");
        assert_eq!(prepared.kind(), micromux::PtyInputKind::Paste);
        assert_eq!(prepared.data(), paste.as_bytes());
        Ok(())
    }

    #[tokio::test]
    async fn bracketed_paste_stays_atomic_when_the_scheduler_queue_is_full() -> eyre::Result<()> {
        let yaml = "version: 1\nservices:\n  svc:\n    command: [\"true\"]\n";
        let mut diagnostics: Vec<Diagnostic<usize>> = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!(err.to_string()))?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let shutdown = micromux::CancellationToken::new();
        let (runner, handles) = mux.start(shutdown.clone());
        assert_eq!(fill_terminal_queue(&handles.terminal)?, 64);
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal),
            shutdown.clone(),
            ViewOptions::default(),
        );
        let paste = vec![b'x'; micromux::MAX_PTY_INPUT_BATCH_BYTES + 17];

        app.enqueue_pty_paste("svc", &paste);

        assert_eq!(app.pending_pty_input.len(), 1);
        let prepared = app
            .pending_pty_input
            .front()
            .ok_or_else(|| eyre::eyre!("paste was not retained"))?;
        assert_eq!(prepared.kind(), micromux::PtyInputKind::Paste);
        assert_eq!(prepared.data(), paste);

        let runner = tokio::spawn(runner);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !app.pending_pty_input.is_empty() {
                app.flush_pending_pty_input();
                tokio::task::yield_now().await;
            }
        })
        .await?;
        shutdown.cancel();
        runner.await??;
        Ok(())
    }

    /// A refused lifecycle command of the local TUI shows its message in the header.
    #[tokio::test]
    async fn a_refused_local_command_shows_why_in_the_header() -> eyre::Result<()> {
        let yaml = "version: 1\nservices:\n  svc:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n    \
                    disabled: true\n";
        let mut diagnostics = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let shutdown = micromux::CancellationToken::new();
        let (runner, handles) = mux.start(shutdown.clone());
        let runner = tokio::spawn(runner);
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal),
            shutdown.clone(),
            ViewOptions::default(),
        );
        let key = |code| KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.resync();

        app.handle_key_press(key(KeyCode::Char('r')));
        tokio::time::timeout(Duration::from_secs(5), app.source.local_notice_changed()).await?;
        let notice = app.source.local_notice();
        assert!(
            notice
                .as_deref()
                .is_some_and(|notice| notice.contains("enable it before restarting")),
            "{notice:?}"
        );
        let area = ratatui::layout::Rect::new(0, 0, 160, 24);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        (&mut app).render(area, &mut buffer);
        let header = (0..area.width)
            .filter_map(|x| buffer.cell((x, 0)).map(|cell| cell.symbol().to_string()))
            .collect::<String>();
        assert!(header.contains("enable it before restarting"), "{header}");

        shutdown.cancel();
        runner.await??;
        Ok(())
    }

    #[tokio::test]
    async fn oversized_paste_is_discarded_with_an_operator_notice() -> eyre::Result<()> {
        let yaml = "version: 1\nservices:\n  svc:\n    command: [\"true\"]\n";
        let mut diagnostics = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let (_runner, handles) = mux.start(micromux::CancellationToken::new());
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal.clone()),
            micromux::CancellationToken::new(),
            ViewOptions::default(),
        );
        let paste = vec![b'x'; micromux::MAX_PTY_PASTE_BYTES + 1];

        app.enqueue_pty_paste("svc", &paste);

        assert!(app.pending_pty_input.is_empty());
        assert!(
            app.input_notice
                .as_deref()
                .is_some_and(|notice| notice.contains("paste discarded"))
        );
        let area = ratatui::layout::Rect::new(0, 0, 160, 24);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        (&mut app).render(area, &mut buffer);
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("paste discarded"));

        app.enqueue_pty_input("svc".to_string(), b"x".to_vec());
        assert!(app.input_notice.is_none());
        assert_eq!(fill_terminal_queue(&handles.terminal)?, 63);
        app.input_notice = Some("stale input warning".to_string());
        app.enqueue_pty_input("svc".to_string(), vec![b'y']);
        assert!(app.input_notice.is_none());
        let paste = vec![b'x'; micromux::MAX_PTY_PASTE_BYTES];
        app.enqueue_pty_paste("svc", &paste);

        assert_eq!(app.pending_pty_input.len(), 2);
        assert!(app.input_notice.is_none());

        app.enqueue_pty_paste("svc", &paste);

        assert_eq!(app.pending_pty_input.len(), 2);
        assert!(
            app.input_notice
                .as_deref()
                .is_some_and(|notice| notice.contains("backlog is full"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn input_drop_notice_survives_a_following_lifecycle_event() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["sh", "-c", "sleep 60"]
                disabled: true
        "#};
        let mut diagnostics = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let shutdown = micromux::CancellationToken::new();
        let (runner, handles) = mux.start(shutdown.clone());
        let reader = handles.reader.clone();
        let commands = handles.commands.clone();
        let terminal = handles.terminal.clone();
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal),
            shutdown.clone(),
            ViewOptions::default(),
        );
        let runner = tokio::spawn(runner);

        let paste = terminal.prepare_paste("svc".to_string(), vec![b'x'])?;
        terminal
            .try_send(paste)
            .map_err(|err| eyre::eyre!("failed to queue test paste: {err:?}"))?;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if reader
                    .events("svc", None, None)
                    .0
                    .iter()
                    .any(|event| event.kind == micromux::ServiceEventKind::InputDropped)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        commands.send(Command::enable("svc".to_string())).await?;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if reader
                    .events("svc", None, None)
                    .0
                    .last()
                    .is_some_and(|event| event.kind != micromux::ServiceEventKind::InputDropped)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;

        app.apply_change(&SessionChange {
            service_id: "svc".to_string(),
            kind: ChangeKind::Events,
        });

        assert!(
            app.input_notice
                .as_deref()
                .is_some_and(|notice| notice.contains("paste discarded"))
        );
        shutdown.cancel();
        runner.await??;
        Ok(())
    }

    #[tokio::test]
    async fn session_warning_outranks_terminal_notices() -> eyre::Result<()> {
        let yaml = "version: 1\nservices:\n  svc:\n    command: [\"true\"]\n";
        let mut diagnostics = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let (_runner, handles) = mux.start(micromux::CancellationToken::new());
        let (commands, _commands_rx) = mpsc::channel(1);
        let mut app = App::new(
            SessionSource::Local(
                LocalSource::new(handles.reader, commands.clone()).with_notice("session warning"),
            ),
            Some(handles.terminal),
            micromux::CancellationToken::new(),
            ViewOptions::default(),
        );
        app.input_notice = Some("input warning".to_string());
        app.terminal_input_closed = true;

        let area = ratatui::layout::Rect::new(0, 0, 160, 24);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        (&mut app).render(area, &mut buffer);
        let text = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(text.contains("session warning"));
        assert!(!text.contains("input warning"));
        assert!(!text.contains(TERMINAL_INPUT_CLOSED_NOTICE));
        Ok(())
    }

    /// A refusal of a start whose resource another session holds shows the command that queues
    /// for it at 120 columns, whatever the holder's name.
    #[tokio::test]
    async fn a_resource_refusal_fits_the_header() -> eyre::Result<()> {
        let yaml = "version: 1\nservices:\n  svc:\n    command: [\"true\"]\n";
        let mut diagnostics = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let (_runner, handles) = mux.start(micromux::CancellationToken::new());
        let (commands, _commands_rx) = mpsc::channel(1);
        let refusal = "resource `desktop-app` is held: queue with `micromux ctl reserve \
                       desktop-app` (MCP: reserve_resource); held by session \
                       feature-model-loader (/home/user/src/feature-model-loader), must renew \
                       within 59m (by 23:22 +01:00); once granted, enable the service";
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, commands).with_notice(refusal)),
            Some(handles.terminal),
            micromux::CancellationToken::new(),
            ViewOptions::default(),
        );

        let area = ratatui::layout::Rect::new(0, 0, 120, 24);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        (&mut app).render(area, &mut buffer);
        let header = (0..area.width)
            .filter_map(|x| buffer.cell((x, 0)).map(ratatui::buffer::Cell::symbol))
            .collect::<String>();
        assert!(
            header.contains("queue with `micromux ctl reserve desktop-app`"),
            "{header}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn tab_cycles_focus_with_and_without_healthcheck_pane() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["sh", "-c", "true"]
        "#};
        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!(err.to_string()))?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(
            test_micromux(&parsed, &cache).map_err(|err| eyre::eyre!(err.to_string()))?,
        );
        let shutdown = micromux::CancellationToken::new();
        // The runner is not spawned; the model reader is seeded with initial snapshots and is all
        // the focus test needs.
        let (_runner, handles) = mux.start(shutdown.clone());

        let mut app = App::new(
            SessionSource::Local(LocalSource::new(
                handles.reader.clone(),
                handles.commands.clone(),
            )),
            Some(handles.terminal.clone()),
            shutdown,
            ViewOptions::default(),
        );
        app.focus = Focus::Services;
        app.side_pane = None;

        app.handle_input_event(tab_press());
        assert_eq!(app.focus, Focus::Logs);
        app.handle_input_event(tab_press());
        assert_eq!(app.focus, Focus::Services);

        app.side_pane = Some(SidePane::Health);
        app.focus = Focus::Services;
        app.handle_input_event(tab_press());
        assert_eq!(app.focus, Focus::Logs);
        app.handle_input_event(tab_press());
        assert_eq!(app.focus, Focus::Side);
        app.handle_input_event(tab_press());
        assert_eq!(app.focus, Focus::Services);

        Ok(())
    }

    #[tokio::test]
    async fn pty_input_and_resize_keys_are_inert_without_an_input_sender() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["sh", "-c", "true"]
        "#};
        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!(err.to_string()))?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let shutdown = micromux::CancellationToken::new();
        let (_runner, handles) = mux.start(shutdown.clone());
        let (commands_tx, mut commands_rx) = mpsc::channel(4);
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, commands_tx)),
            None,
            shutdown,
            ViewOptions::default(),
        );
        let key = |code| KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        app.handle_key_press(key(KeyCode::Char('a')));
        assert!(!app.pty_input_mode);

        // Even an internally inconsistent state cannot route bytes without the capability.
        app.pty_input_mode = true;
        app.handle_key_press(key(KeyCode::Char('x')));
        app.handle_crossterm_event(&crossterm::event::Event::Resize(120, 40));

        assert_matches!(
            commands_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        );
        let area = ratatui::layout::Rect::new(0, 0, 160, 24);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        (&mut app).render(area, &mut buffer);
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(!rendered.contains("PTY input"));
        assert!(!rendered.contains("Exit PTY input"));
        Ok(())
    }

    fn pane_size(app: &App) -> TerminalSize {
        let (cols, rows) = app.desired_pty_size();
        TerminalSize { cols, rows }
    }

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// The header's controls are placed in the cells the terminal draws them in, so beside a
    /// session path with halfwidth katakana sound marks, which take a cell more than their
    /// unicode width, each control is drawn where a click on it lands, and the last one whole.
    #[tokio::test]
    async fn header_controls_are_clicked_where_they_are_drawn() -> eyre::Result<()> {
        let (source, _forwarding) = RemoteSource::offline();
        source.set_config_path("/ｶﾞｶﾞｶﾞｶﾞ/micromux.yaml");
        let terminal = source.terminal();
        let mut app = App::new(
            SessionSource::Remote(source),
            terminal,
            micromux::CancellationToken::new(),
            ViewOptions::default(),
        );
        let area = ratatui::layout::Rect::new(0, 0, 140, 20);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(&mut app, area, &mut buffer);
        let row = (0..area.width)
            .filter_map(|x| buffer.cell((x, 0)).map(ratatui::buffer::Cell::symbol))
            .collect::<Vec<_>>();
        let header = app.hits.hints.iter().filter(|hit| hit.area.y == 0);
        let mut clicked = 0;
        for hit in header {
            let shown = row
                .get(usize::from(hit.area.x)..)
                .unwrap_or_default()
                .concat();
            assert!(shown.starts_with(hit.keys), "{} under {shown:?}", hit.keys);
            clicked += 1;
        }
        assert!(clicked > 0, "{}", row.concat());
        assert!(
            row.concat().trim_end().ends_with("Pane:OFF  A All:OFF"),
            "{}",
            row.concat()
        );
        Ok(())
    }

    /// Watching a session never resizes its PTYs: an attached TUI resizes them only while it
    /// types into a service, starting with its current size when it enters PTY input mode.
    #[tokio::test]
    async fn an_attached_tui_resizes_only_in_pty_input_mode() -> eyre::Result<()> {
        let (source, mut forwarding) = RemoteSource::offline();
        let terminal = source.terminal();
        let mut app = App::new(
            SessionSource::Remote(source),
            terminal,
            micromux::CancellationToken::new(),
            ViewOptions::default(),
        );

        // Attaching and resizing the terminal leave the session's PTYs alone.
        app.maybe_resize_pty();
        app.handle_crossterm_event(&crossterm::event::Event::Resize(120, 40));
        assert!(!forwarding.size.has_changed()?);

        // Entering PTY input mode claims the PTYs for this pane, and pane changes follow.
        app.handle_key_press(press(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(app.pty_input_mode);
        assert_eq!(*forwarding.size.borrow_and_update(), Some(pane_size(&app)));
        app.handle_crossterm_event(&crossterm::event::Event::Resize(100, 30));
        assert_eq!(*forwarding.size.borrow_and_update(), Some(pane_size(&app)));

        // Leaving it withdraws the size, so a reconnect to a replacement session, which marks
        // the size changed to re-send it, finds nothing to send, and pane changes send nothing.
        app.handle_key_press(press(KeyCode::Char(']'), KeyModifiers::CONTROL));
        assert!(!app.pty_input_mode);
        assert_eq!(*forwarding.size.borrow_and_update(), None);
        forwarding.size.mark_changed();
        assert_eq!(*forwarding.size.borrow_and_update(), None);
        app.handle_crossterm_event(&crossterm::event::Event::Resize(140, 50));
        assert!(!forwarding.size.has_changed()?);

        // Entering the mode again sends the current size once more.
        app.handle_key_press(press(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(*forwarding.size.borrow_and_update(), Some(pane_size(&app)));
        Ok(())
    }

    /// The session's own TUI follows its pane as before, and takes the PTY size back when it
    /// enters PTY input mode, even when its pane did not change since an attached TUI resized.
    #[tokio::test]
    async fn the_local_tui_reasserts_its_size_in_pty_input_mode() -> eyre::Result<()> {
        let mut diagnostics = Vec::new();
        let parsed = micromux::from_str(
            "version: 1\nservices: {}\n",
            Path::new("."),
            0usize,
            None,
            &mut diagnostics,
        )?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
        let shutdown = micromux::CancellationToken::new();
        let (_runner, handles) = mux.start(shutdown.clone());
        // A forwarding capability stands in for the scheduler, so the test sees each resize.
        let (terminal, mut forwarding) = TerminalControl::forwarding(micromux::MAX_PTY_PASTE_BYTES);
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(terminal),
            shutdown,
            ViewOptions::default(),
        );

        app.handle_crossterm_event(&crossterm::event::Event::Resize(120, 40));
        let size = *forwarding.size.borrow_and_update();
        assert_eq!(size, Some(pane_size(&app)));

        app.handle_key_press(press(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(forwarding.size.has_changed()?);
        assert_eq!(*forwarding.size.borrow_and_update(), size);
        Ok(())
    }

    /// Input an attached session cannot take while it reconnects is discarded with a notice,
    /// together with input already waiting behind a full queue, instead of being sent later.
    #[tokio::test]
    async fn input_refused_while_reconnecting_is_discarded() -> eyre::Result<()> {
        let (source, mut forwarding) = RemoteSource::offline();
        let terminal = source
            .terminal()
            .ok_or_else(|| eyre::eyre!("the offline session accepts terminal input"))?;
        let mut app = App::new(
            SessionSource::Remote(source),
            Some(terminal.clone()),
            micromux::CancellationToken::new(),
            ViewOptions::default(),
        );

        // A full queue makes the TUI keep further input pending.
        let queued = fill_terminal_queue(&terminal)?;
        app.enqueue_pty_input("svc".to_string(), b"pending".to_vec());
        assert_eq!(app.pending_pty_input.len(), 1);

        // The connection breaks: pending input is dropped on the next retry, and new input at
        // once, both without reaching the queue.
        forwarding.set_accepting(false);
        app.flush_pending_pty_input();
        assert!(app.pending_pty_input.is_empty());
        app.enqueue_pty_input("svc".to_string(), b"typed".to_vec());
        app.enqueue_pty_paste("svc", b"pasted");
        assert!(app.pending_pty_input.is_empty());
        assert_eq!(app.input_notice.as_deref(), Some(DISCONNECTED_INPUT_NOTICE));

        let mut forwarded = 0;
        while let Ok(input) = forwarding.input.try_recv() {
            assert_eq!(input.data(), b"x");
            forwarded += 1;
        }
        assert_eq!(forwarded, queued);
        Ok(())
    }

    /// A disabled service's log pane keeps its ANSI shapes but shows them in gray.
    #[tokio::test]
    async fn disabled_service_logs_render_in_gray() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["sh", "-c", "true"]
        "#};
        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!(err.to_string()))?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(
            test_micromux(&parsed, &cache).map_err(|err| eyre::eyre!(err.to_string()))?,
        );
        let shutdown = micromux::CancellationToken::new();
        let (_runner, handles) = mux.start(shutdown.clone());
        let (commands_tx, _commands_rx) = mpsc::channel(4);
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader.clone(), commands_tx)),
            Some(handles.terminal),
            shutdown,
            ViewOptions::default(),
        );

        // Hand the viewer one red, bold line as if the service had printed it.
        // Recording the format first keeps the render from rebuilding the lines from the empty
        // log.
        let pretty_json_logs = app.pretty_json_logs;
        let service = app
            .state
            .current_service_mut()
            .ok_or_else(|| eyre::eyre!("no current service"))?;
        let format = service.line_format(pretty_json_logs);
        service.use_format(&format);
        service.apply_log_records(
            None,
            &[micromux::LogLine {
                seq: 1,
                run_generation: 1,
                timestamp_unix_ms: 0,
                line: "\x1b[1;31mfrozen\x1b[0m".to_string(),
            }],
            &format,
        );

        let area = ratatui::layout::Rect::new(0, 0, 120, 12);
        let render = |app: &mut App| -> eyre::Result<ratatui::buffer::Cell> {
            let mut buffer = ratatui::buffer::Buffer::empty(area);
            app.render(area, &mut buffer);
            // Find the first cell of the word inside the log pane.
            let rows = area
                .rows()
                .map(|row| {
                    row.columns()
                        .filter_map(|column| buffer.cell(column).map(ratatui::buffer::Cell::symbol))
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            let (y, offset) = rows
                .iter()
                .zip(0u16..)
                .find_map(|(text, y)| {
                    // The border glyphs before the word are multi-byte, so count columns.
                    let byte = text.find("frozen")?;
                    Some((y, text[..byte].chars().count()))
                })
                .ok_or_else(|| eyre::eyre!("log line not rendered:\n{}", rows.join("\n")))?;
            let x = u16::try_from(offset)?;
            buffer
                .cell((x, y))
                .cloned()
                .ok_or_else(|| eyre::eyre!("log cell missing"))
        };

        // Live, the line keeps its red.
        let live = render(&mut app)?;
        assert_eq!(live.fg, ratatui::style::Color::Red);
        assert!(live.modifier.contains(ratatui::style::Modifier::BOLD));

        // Disabled, the same line is a gray that still carries its bold.
        if let Some(service) = app.state.current_service_mut() {
            service.snapshot.desired = micromux::Desired::Disabled;
        }
        let frozen = render(&mut app)?;
        assert_eq!(frozen.fg, ratatui::style::Color::DarkGray);
        assert!(frozen.modifier.contains(ratatui::style::Modifier::BOLD));
        Ok(())
    }

    #[tokio::test]
    async fn restart_key_on_disabled_service_does_not_send_enable() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["sh", "-c", "true"]
        "#};
        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!(err.to_string()))?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(
            test_micromux(&parsed, &cache).map_err(|err| eyre::eyre!(err.to_string()))?,
        );
        let shutdown = micromux::CancellationToken::new();
        let (_runner, handles) = mux.start(shutdown.clone());
        let (commands_tx, mut commands_rx) = mpsc::channel(4);

        let mut app = App::new(
            SessionSource::Local(LocalSource::new(
                handles.reader.clone(),
                commands_tx.clone(),
            )),
            Some(handles.terminal),
            shutdown,
            ViewOptions::default(),
        );
        if let Some(service) = app.state.current_service_mut() {
            service.snapshot.desired = micromux::Desired::Disabled;
        }

        app.restart_current_service();

        let command = tokio::time::timeout(Duration::from_secs(1), commands_rx.recv())
            .await?
            .ok_or_else(|| eyre::eyre!("lifecycle relay stopped"))?;
        match command {
            micromux::Command::Restart { service, .. } => assert_eq!(service, "svc"),
            other => {
                eyre::bail!("expected restart command, got {other:?}");
            }
        }
        assert_matches!(
            commands_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        );

        Ok(())
    }

    #[tokio::test]
    async fn stop_key_only_sends_for_a_live_dynamic_service() -> eyre::Result<()> {
        let yaml = indoc! {r#"
            version: 1
            services:
              svc:
                command: ["sh", "-c", "true"]
        "#};
        let mut diagnostics: Vec<Diagnostic<usize>> = vec![];
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)
            .map_err(|err| eyre::eyre!(err.to_string()))?;
        let cache = tempfile::tempdir()?;
        let mux = std::sync::Arc::new(
            test_micromux(&parsed, &cache).map_err(|err| eyre::eyre!(err.to_string()))?,
        );
        let shutdown = micromux::CancellationToken::new();
        let (_runner, handles) = mux.start(shutdown.clone());
        let (commands_tx, mut commands_rx) = mpsc::channel(4);
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(
                handles.reader.clone(),
                commands_tx.clone(),
            )),
            Some(handles.terminal),
            shutdown,
            ViewOptions::default(),
        );
        let stop_key = KeyEvent {
            code: KeyCode::Char('s'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };

        app.handle_key_press(stop_key);
        assert_matches!(
            commands_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        );

        if let Some(service) = app.state.current_service_mut() {
            service.snapshot.origin = micromux::OriginKind::Dynamic;
            service.snapshot.dynamic = Some(micromux::DynamicServiceInfo {
                created_at_unix_ms: 1,
                expires_at_unix_ms: None,
                owner: None,
                revision: 1,
            });
        }
        app.handle_key_press(stop_key);
        let command = tokio::time::timeout(Duration::from_secs(1), commands_rx.recv())
            .await?
            .ok_or_else(|| eyre::eyre!("lifecycle relay stopped"))?;
        match command {
            micromux::Command::StopDynamic {
                caller,
                service,
                ack,
            } => {
                assert_eq!(caller, micromux::Caller::Local);
                assert_eq!(service, "svc");
                assert!(ack.is_some());
            }
            other => {
                eyre::bail!("expected stop-dynamic command, got {other:?}");
            }
        }

        if let Some(service) = app.state.current_service_mut() {
            service.snapshot.retired = Some(micromux::RetiredReason::Stopped);
        }
        app.handle_key_press(stop_key);
        assert_matches!(
            commands_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        );

        Ok(())
    }

    #[tokio::test]
    async fn resync_reconciles_rows_and_preserves_selection_by_id() -> eyre::Result<()> {
        fn handles_for(yaml: &str) -> eyre::Result<micromux::Handles> {
            let mut diagnostics: Vec<Diagnostic<usize>> = Vec::new();
            let parsed = micromux::from_str(yaml, Path::new("."), 0, None, &mut diagnostics)
                .map_err(|err| eyre::eyre!(err.to_string()))?;
            let cache = tempfile::tempdir()?;
            let mux = std::sync::Arc::new(test_micromux(&parsed, &cache)?);
            let (_runner, handles) = mux.start(micromux::CancellationToken::new());
            Ok(handles)
        }

        let initial = handles_for(indoc! {r#"
            version: 1
            services:
              a: { command: ["true"] }
              b: { command: ["true"] }
        "#})?;
        let shutdown = micromux::CancellationToken::new();
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(initial.reader, initial.commands.clone())),
            Some(initial.terminal),
            shutdown,
            ViewOptions::default(),
        );
        app.state.selected_service = 1;
        if let Some(service) = app.state.current_service_mut() {
            service.log.text = ratatui::text::Text::from("preserved");
        }

        let updated = handles_for(indoc! {r#"
            version: 1
            services:
              b: { command: ["true"] }
              c: { command: ["true"] }
        "#})?;
        app.source = SessionSource::Local(LocalSource::new(updated.reader, updated.commands));
        app.resync();

        assert_eq!(
            app.state
                .services
                .iter()
                .map(|service| service.snapshot.id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c"]
        );
        assert_eq!(app.state.selected_service, 0);
        assert_eq!(
            app.state
                .current_service()
                .map(|service| service.log.text.clone()),
            Some(ratatui::text::Text::from("preserved"))
        );
        Ok(())
    }
}
