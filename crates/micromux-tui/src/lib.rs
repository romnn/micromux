//! `micromux-tui` provides the terminal user interface for micromux.
//!
//! The main entry point is [`App`], constructed from a [`SessionSource`], an optional PTY-input
//! sender, a shutdown token, and display options; call [`App::render`] to run it. The TUI holds only
//! view state and re-reads its synchronous source on each [`micromux::SessionChange`].

mod clipboard;
mod event;
mod json_log;
mod level;
mod mouse;
mod remote;
mod render;
mod source;
mod state;
mod style;
mod terminal;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseOptions {
    /// Capture the mouse for clicks, the wheel and selecting text inside one pane.
    ///
    /// Without it the terminal keeps its own mouse handling, such as selecting text across the
    /// whole screen; the `m` key switches capture at runtime either way.
    pub capture: bool,
    /// Copy a selection to the clipboard as soon as the drag ends, rather than only on an
    /// explicit copy key.
    pub copy_on_select: bool,
}

impl Default for MouseOptions {
    fn default() -> Self {
        Self {
            capture: true,
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
    OpenUrl(String),
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
    /// Event handler
    input_event_handler: event::InputHandler,
    /// Current state
    state: state::State,
    /// Log viewer
    log_view: crate::render::log_view::LogView,
    healthcheck_view: crate::render::log_view::LogView,
    show_healthcheck_pane: bool,
    pretty_json_logs: bool,
    level_picker: Option<level::LevelPicker>,
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
    /// Healthcheck pane is focused.
    Healthcheck,
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
        let healthcheck_view = render::log_view::LogView::default();

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
            input_event_handler: event::InputHandler::new(),
            state: state::State::new(services),
            log_view,
            healthcheck_view,
            show_healthcheck_pane: false,
            pretty_json_logs: view_options.pretty_json,
            level_picker: None,
            show_help: false,
            info: None,
            popup_scroll: 0,
            mouse: mouse::MouseState::new(view_options.mouse),
            first_service: 0,
            quit_hint_at: None,
            hits: mouse::Hits::default(),
            effects: Vec::new(),
            pty_input_mode: false,
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

    fn maybe_resize_pty(&mut self) {
        let Some(input) = &self.input else {
            return;
        };
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
            }
        }
    }

    /// Run the TUI event loop.
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying terminal backend fails to draw.
    pub async fn run(mut self, mut terminal: DefaultTerminal) -> Result<(), Error> {
        enum Wake {
            Input(event::Input),
            Change(SessionChange),
            /// The change broadcast lagged (or this is the initial draw): re-read everything.
            Resync,
            /// Queued PTY input is due for another try, or the quit prompt for one.
            Timer,
            /// The session's URLs or routing status changed; only the header needs a redraw.
            Summary,
            /// The process continued after a stop, and the shell may have taken the terminal.
            Continued,
        }

        let mut continued = terminal::Continued::watch();

        let area = terminal.size()?;
        self.terminal_cols = area.width;
        self.terminal_rows = area.height;
        self.state
            .clamp_sidebar(self.terminal_cols.saturating_sub(22));
        self.maybe_resize_pty();

        // Seed the view from the model before the first frame.
        self.resync();
        terminal.draw(|frame| frame.render_widget(&mut self, frame.area()))?;
        let mut last_frame = tokio::time::Instant::now();

        while self.is_running() {
            let timer = self.next_timer();
            let wake = tokio::select! {
                () = self.shutdown.cancelled() => None,
                input = self.input_event_handler.next() => {
                    Some(Wake::Input(input))
                },
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
                () = continued.recv() => Some(Wake::Continued),
            };

            let mut needs_resync = false;
            match wake {
                Some(Wake::Input(event)) => self.handle_input_event(event),
                Some(Wake::Change(change)) => needs_resync |= self.apply_change(&change),
                Some(Wake::Resync) => needs_resync = true,
                Some(Wake::Timer) => self.on_timer(std::time::Instant::now()),
                Some(Wake::Summary) => {}
                Some(Wake::Continued) => self.take_terminal_again(&mut terminal)?,
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
            #[cfg(unix)]
            if self.apply_effects(&mut terminal)? {
                self.suspend(&mut terminal, &mut continued).await?;
            }
            #[cfg(not(unix))]
            self.apply_effects(&mut terminal)?;
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
            terminal.draw(|frame| frame.render_widget(&mut self, frame.area()))?;
            last_frame = tokio::time::Instant::now();
        }
        Ok(())
    }

    /// When the event loop must wake by itself: to retry queued PTY input, or to take back the
    /// quit hint.
    fn next_timer(&self) -> Option<std::time::Instant> {
        let retry = (!self.pending_pty_input.is_empty())
            .then(|| std::time::Instant::now() + PENDING_PTY_INPUT_RETRY);
        let hint = self.quit_hint_at.map(|at| at + QUIT_HINT_DURATION);
        retry.into_iter().chain(hint).min()
    }

    /// Does what is due `now`: retries queued PTY input and takes back an expired quit hint.
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
    }

    /// Ctrl-C pressed `now`, which never quits: only `q` does, so a stray Ctrl-C, as from a copy
    /// shortcut a terminal sends as Ctrl-C, cannot stop the session. The footer says so for a
    /// moment.
    fn ctrl_c(&mut self, now: std::time::Instant) {
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
                Effect::OpenUrl(url) => {
                    if let Err(err) = open_url(&url) {
                        tracing::warn!(?err, url, "cannot open the URL in a browser");
                        self.mouse.notice = Some(format!("cannot open a browser: {err}"));
                    }
                }
                Effect::Suspend => suspend = true,
            }
        }
        Ok(suspend)
    }

    /// Suspends to the shell as Ctrl-Z does there, then takes the terminal again once the shell
    /// continues micromux.
    #[cfg(unix)]
    async fn suspend(
        &mut self,
        terminal: &mut DefaultTerminal,
        continued: &mut terminal::Continued,
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

        terminal::take_over(self.mouse.capture)?;
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
                    service.snapshot = snapshot;
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
            ChangeKind::Events => self.observe_input_events(&change.service_id),
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
        use crossterm::event::KeyEventKind;

        if let crossterm::event::Event::Key(key) = *event
            && terminal::debug_keys()
        {
            tracing::warn!(
                target: "micromux::keys",
                code = ?key.code,
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
                self.mouse.notice = None;
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

            // Drop or copy the mouse selection
            KeyCode::Esc => self.mouse.selection = None,
            KeyCode::Char('y') => self.copy_selection(),
            KeyCode::Enter if self.has_selection() => self.copy_selection(),

            // Toggle focus
            KeyCode::Tab => self.toggle_focus(),

            // Toggle mouse capture
            KeyCode::Char('m') => self.toggle_mouse_capture(),

            // Toggle PTY input mode
            KeyCode::Char('a') if self.input.is_some() => {
                self.pty_input_mode = !self.pty_input_mode;
            }
            KeyCode::Char('H') => self.toggle_healthcheck_pane(),

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

    fn handle_key_press_pty_input_mode(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};

        match key.code {
            KeyCode::Esc if key.modifiers.contains(KeyModifiers::ALT) => {
                self.pty_input_mode = false;
            }
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
        self.focus = if self.show_healthcheck_pane {
            match self.focus {
                Focus::Services => Focus::Logs,
                Focus::Logs => Focus::Healthcheck,
                Focus::Healthcheck => Focus::Services,
            }
        } else {
            match self.focus {
                Focus::Services => Focus::Logs,
                Focus::Logs | Focus::Healthcheck => Focus::Services,
            }
        };
    }

    fn toggle_healthcheck_pane(&mut self) {
        self.show_healthcheck_pane = !self.show_healthcheck_pane;
        if !self.show_healthcheck_pane && self.focus == Focus::Healthcheck {
            self.focus = Focus::Logs;
        }
        self.maybe_resize_pty();
    }

    fn navigate_up(&mut self) {
        match self.focus {
            Focus::Services => self.state.service_up(),
            Focus::Logs => self.scroll_logs_up(1),
            Focus::Healthcheck => self.scroll_healthchecks_up(1),
        }
    }

    fn navigate_down(&mut self) {
        match self.focus {
            Focus::Services => self.state.service_down(),
            Focus::Logs => self.scroll_logs_down(1),
            Focus::Healthcheck => self.scroll_healthchecks_down(1),
        }
    }

    fn scroll_to_top(&mut self) {
        match self.focus {
            Focus::Logs => {
                self.log_view.follow_tail = false;
                self.log_view.scroll_offset = 0;
            }
            Focus::Healthcheck => {
                self.healthcheck_view.follow_tail = false;
                self.healthcheck_view.scroll_offset = 0;
            }
            Focus::Services => {}
        }
    }

    fn scroll_to_bottom(&mut self) {
        match self.focus {
            Focus::Logs => {
                self.log_view.follow_tail = true;
            }
            Focus::Healthcheck => {
                self.healthcheck_view.follow_tail = true;
            }
            Focus::Services => {}
        }
    }

    fn toggle_wrap(&mut self) {
        let wrap = !self.log_view.wrap;
        self.log_view.wrap = wrap;
        self.healthcheck_view.wrap = wrap;
    }

    fn toggle_tail(&mut self) {
        match self.focus {
            Focus::Logs | Focus::Services => {
                self.log_view.follow_tail = !self.log_view.follow_tail;
            }
            Focus::Healthcheck => {
                self.healthcheck_view.follow_tail = !self.healthcheck_view.follow_tail;
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
        let Some(service) = self.state.current_service() else {
            return;
        };
        // Wrap-aware count persisted by the renderer, so scrolling reaches the true bottom
        // even when wrapping is enabled (the raw entry count would stop short).
        let total = service.cached_line_index.total_lines();
        let viewport = usize::from(self.log_viewport_height());
        self.log_view.scroll_by(rows, total, viewport);
    }

    fn scroll_healthchecks_up(&mut self, lines: u16) {
        self.scroll_healthchecks(-isize::from(lines.cast_signed()));
    }

    fn scroll_healthchecks_down(&mut self, lines: u16) {
        self.scroll_healthchecks(isize::from(lines.cast_signed()));
    }

    /// Scrolls the healthcheck pane by `rows`, up when negative.
    fn scroll_healthchecks(&mut self, rows: isize) {
        let Some(service) = self.state.current_service() else {
            return;
        };
        let total = service.healthcheck_cached_line_index.total_lines();
        let viewport = usize::from(self.log_viewport_height());
        self.healthcheck_view.scroll_by(rows, total, viewport);
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

/// Opens `url` in the browser with the platform's opener, without waiting for it.
///
/// The opener's output is discarded, since anything it prints would land on the TUI.
fn open_url(url: &str) -> std::io::Result<()> {
    use std::process::{Command, Stdio};

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
    clipboard::detach(&mut command);
    let mut child = command.spawn()?;
    // Reap the opener once it exits, so it does not linger as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
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
/// micromux may have asked its own terminal for the kitty protocol, which reports keys such as
/// Ctrl+Shift+C or Ctrl+I apart that legacy terminals send alike; this folds them back into the
/// legacy bytes. Super and the other modifiers legacy terminals cannot send are left out.
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
    use micromux::Command;
    use ratatui::widgets::Widget as _;
    use similar_asserts::assert_eq;
    use tokio::sync::mpsc;

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
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
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
        assert!(app.show_healthcheck_pane);

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
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
        let shutdown = micromux::CancellationToken::new();
        let (_runner, handles) = mux.start(shutdown.clone());
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal.clone()),
            shutdown.clone(),
            ViewOptions::default(),
        );
        let now = std::time::Instant::now();
        // Narrow enough that the hint has no free room in the footer and covers its last row.
        let footer = |app: &mut App| {
            let area = ratatui::layout::Rect::new(0, 0, 60, 20);
            let mut buffer = ratatui::buffer::Buffer::empty(area);
            ratatui::widgets::Widget::render(&mut *app, area, &mut buffer);
            (0..60)
                .filter_map(|x| buffer.cell((x, 19)).map(ratatui::buffer::Cell::symbol))
                .collect::<String>()
        };

        // A press says how to quit, without stopping anything.
        app.ctrl_c(now);
        assert!(app.running);
        assert!(footer(&mut app).contains("press q to quit"));

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
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
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
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
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
            "Health:OFF",
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
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
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
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
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

    /// A local app for a session established from `yaml` in `directory`, with its live session
    /// status and allocated ports.
    #[cfg(unix)]
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
        let mux = std::sync::Arc::new(micromux::Micromux::with_session(&parsed, session)?);
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
    #[cfg(unix)]
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

        // A narrower pane shows the URLs that fit whole and counts the rest, on the one title row,
        // and the filters keep their corner.
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
    #[cfg(unix)]
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

        // Both ends of the second URL open it, path included, and the separator between the URLs
        // opens nothing.
        let api = "http://app.tui.localhost:8080/api/";
        let start = column(api)?;
        let width = u16::try_from(api.len())?;
        click(&mut app, start);
        click(&mut app, start + width - 1);
        click(&mut app, start - 2);
        assert_eq!(
            std::mem::take(&mut app.effects),
            vec![
                Effect::OpenUrl(api.to_string()),
                Effect::OpenUrl(api.to_string())
            ]
        );
        click(&mut app, column("http://app.tui.localhost:8080")?);
        assert_eq!(
            app.effects,
            vec![Effect::OpenUrl("http://app.tui.localhost:8080".to_string())]
        );
        Ok(())
    }

    /// The whole screen the app renders at `width` × `height`, one line per row.
    #[cfg(unix)]
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
    #[cfg(unix)]
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
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
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
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
        let (_runner, handles) = mux.start(micromux::CancellationToken::new());
        let mut app = App::new(
            SessionSource::Local(LocalSource::new(handles.reader, handles.commands.clone())),
            Some(handles.terminal.clone()),
            micromux::CancellationToken::new(),
            ViewOptions::default(),
        );
        app.show_healthcheck_pane = true;
        let area = ratatui::layout::Rect::new(0, 0, 120, 30);
        let mut buffer = ratatui::buffer::Buffer::empty(area);

        (&mut app).render(area, &mut buffer);
        let cached_pointer = app
            .state
            .current_service()
            .and_then(|service| service.healthcheck_cached_text.lines.first())
            .and_then(|line| line.spans.first())
            .map(|span| span.content.as_ptr())
            .ok_or_else(|| eyre::eyre!("healthcheck cache was empty"))?;
        (&mut app).render(area, &mut buffer);
        let reused_pointer = app
            .state
            .current_service()
            .and_then(|service| service.healthcheck_cached_text.lines.first())
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
    /// reported them in legacy form or through the kitty keyboard protocol, which reports some
    /// apart that legacy terminals send alike.
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
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
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
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
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
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
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

    #[tokio::test]
    async fn oversized_paste_is_discarded_with_an_operator_notice() -> eyre::Result<()> {
        let yaml = "version: 1\nservices:\n  svc:\n    command: [\"true\"]\n";
        let mut diagnostics = Vec::new();
        let parsed = micromux::from_str(yaml, Path::new("."), 0usize, None, &mut diagnostics)?;
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
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
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
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
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
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
        let mux = std::sync::Arc::new(
            micromux::Micromux::new(&parsed).map_err(|err| eyre::eyre!(err.to_string()))?,
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
        app.show_healthcheck_pane = false;

        app.handle_input_event(tab_press());
        assert_eq!(app.focus, Focus::Logs);
        app.handle_input_event(tab_press());
        assert_eq!(app.focus, Focus::Services);

        app.show_healthcheck_pane = true;
        app.focus = Focus::Services;
        app.handle_input_event(tab_press());
        assert_eq!(app.focus, Focus::Logs);
        app.handle_input_event(tab_press());
        assert_eq!(app.focus, Focus::Healthcheck);
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
        let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
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
        let mux = std::sync::Arc::new(
            micromux::Micromux::new(&parsed).map_err(|err| eyre::eyre!(err.to_string()))?,
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
        assert!(
            matches!(frozen.fg, ratatui::style::Color::Rgb(red, green, blue) if red == green && green == blue),
            "{:?}",
            frozen.fg
        );
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
        let mux = std::sync::Arc::new(
            micromux::Micromux::new(&parsed).map_err(|err| eyre::eyre!(err.to_string()))?,
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
        let mux = std::sync::Arc::new(
            micromux::Micromux::new(&parsed).map_err(|err| eyre::eyre!(err.to_string()))?,
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
            micromux::Command::StopDynamic { service, ack } => {
                assert_eq!(service, "svc");
                assert!(ack.is_none());
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
            let mux = std::sync::Arc::new(micromux::Micromux::new(&parsed)?);
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
            service.cached_text = ratatui::text::Text::from("preserved");
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
                .map(|service| service.cached_text.clone()),
            Some(ratatui::text::Text::from("preserved"))
        );
        Ok(())
    }
}
