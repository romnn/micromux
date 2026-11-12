//! Client-side session mirror used by attach mode.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use indexmap::IndexMap;
use micromux::{
    CancellationToken, ChangeKind, HealthAttempt, LogLine, PreparedPtyInput, PtyInputKind,
    ServiceCommandAck, ServiceID, ServiceSnapshot, SessionChange, TerminalControl,
    TerminalForwarding, TerminalSize,
};
use micromux_control::{
    Client, ControlEndpoint, ControlError, EVENTS_PROTOCOL_VERSION, ErrorCode, InputBytes,
    MAX_REMOTE_PASTE_BYTES, Request, Response, SessionInfo, Subscription,
    TERMINAL_PROTOCOL_VERSION,
};
use parking_lot::RwLock;
use tokio::sync::{broadcast, mpsc};

use crate::source::AttachmentStatus;
use crate::timeline::Timeline;

const CHANGE_CHANNEL_CAPACITY: usize = 1024;
const REMOTE_LOG_TAIL: usize = 200;
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(250);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(5);
/// Connected lifetime after which a reconnect is stable enough to reset its backoff.
const STABLE_CONNECTION_DURATION: Duration = Duration::from_secs(30);
/// How often an attached TUI re-reads the session description for its URLs and routing status.
const SESSION_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// A synchronous mirror of a remote session, maintained by one background task.
///
/// The same task forwards PTY input and resizes from [`RemoteSource::terminal`] to the session.
pub struct RemoteSource {
    store: Arc<RwLock<MirrorStore>>,
    changes: broadcast::Sender<SessionChange>,
    commands: mpsc::UnboundedSender<RemoteCommand>,
    terminal: TerminalControl,
    shutdown: CancellationToken,
}

struct MirrorStore {
    session: SessionInfo,
    services: IndexMap<ServiceID, MirrorEntry>,
    connected: bool,
    notice: Option<String>,
}

struct MirrorEntry {
    snapshot: ServiceSnapshot,
    logs: VecDeque<LogLine>,
    next_log_seq: u64,
    first_retained_seq: Option<u64>,
    health: Vec<HealthAttempt>,
    events: Timeline,
    // Manual restarts clear visible logs without resetting their sequence. The acknowledged
    // generation distinguishes that clear from an automatic restart, which retains logs.
    clear_logs_after_generation: Option<u64>,
}

/// What the TUI asks of the session, drained by the mirror task.
struct RemoteInbox {
    commands: mpsc::UnboundedReceiver<RemoteCommand>,
    terminal: TerminalForwarding,
}

enum RemoteCommand {
    Restart(ServiceID),
    RestartAll,
    Enable(ServiceID),
    Disable(ServiceID),
    StopDynamic(ServiceID),
}

enum ExpectedCommandResponse {
    Accepted,
    RestartAccepted,
    Dynamic,
}

enum ConnectedLoopExit {
    Shutdown,
    Reconnect,
}

struct ReconnectBackoff {
    next: Duration,
}

impl ReconnectBackoff {
    fn new() -> Self {
        Self {
            next: INITIAL_RECONNECT_DELAY,
        }
    }

    fn next_delay(&mut self) -> Duration {
        let current = self.next;
        self.next = self.next.saturating_mul(2).min(MAX_RECONNECT_DELAY);
        current
    }

    fn reset(&mut self) {
        self.next = INITIAL_RECONNECT_DELAY;
    }
}

struct CoalescedChanges(Vec<SessionChange>);

impl CoalescedChanges {
    fn new(changes: Vec<SessionChange>) -> Self {
        let mut seen = HashSet::with_capacity(changes.len());
        Self(
            changes
                .into_iter()
                .filter(|change| seen.insert((change.service_id.clone(), change.kind)))
                .collect(),
        )
    }

    fn iter(&self) -> impl Iterator<Item = &SessionChange> {
        self.0.iter()
    }
}

trait RequestConnection {
    async fn request(&mut self, request: Request) -> Result<Response, ControlError>;
}

impl RequestConnection for Client {
    async fn request(&mut self, request: Request) -> Result<Response, ControlError> {
        Client::request(self, request).await
    }
}

impl RemoteSource {
    /// Connect to a session, populate the mirror, and start its refresh task.
    ///
    /// # Errors
    ///
    /// Returns a typed control error if the initial connection, protocol check, subscription, or
    /// mirror population fails. Once connected, transport failures are handled by reconnecting in
    /// the background.
    pub async fn connect(endpoint: ControlEndpoint) -> Result<Self, ControlError> {
        let mut request = Client::connect(&endpoint).await?;
        let session = request.describe().await?;
        let store = Arc::new(RwLock::new(MirrorStore {
            session,
            services: IndexMap::new(),
            connected: false,
            notice: None,
        }));
        let (changes, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        let (subscription, service_ids) =
            subscribe_without_sync_gap(&endpoint, &mut request, &store).await?;
        store.write().connected = true;
        publish_full_refresh(&changes, &service_ids);

        let (commands, command_rx) = mpsc::unbounded_channel();
        let (terminal, forwarding) = TerminalControl::forwarding(MAX_REMOTE_PASTE_BYTES);
        let shutdown = CancellationToken::new();
        tokio::spawn(run_remote(
            endpoint,
            Arc::clone(&store),
            changes.clone(),
            RemoteInbox {
                commands: command_rx,
                terminal: forwarding,
            },
            shutdown.clone(),
            request,
            subscription,
        ));

        Ok(Self {
            store,
            changes,
            commands,
            terminal,
            shutdown,
        })
    }

    /// Returns a capability that types into the session's services and resizes their PTYs.
    ///
    /// It is `None` when the session predates [`TERMINAL_PROTOCOL_VERSION`].
    /// Input reaches the session in the order it was sent, over the same connection as the
    /// mirror's lifecycle commands, and other clients' input interleaves with it only between
    /// requests.
    /// The last resize the session receives, from any client, sets every PTY's size.
    /// While the mirror is reconnecting the capability refuses input, so the TUI discards it
    /// rather than replaying it later.
    #[must_use]
    pub fn terminal(&self) -> Option<TerminalControl> {
        accepts_terminal(&self.store.read().session).then(|| self.terminal.clone())
    }

    pub(crate) fn attachment_status(&self) -> AttachmentStatus {
        let store = self.store.read();
        AttachmentStatus {
            session: store.session.clone(),
            connected: store.connected,
            notice: store.notice.clone(),
        }
    }

    /// Return every mirrored service in server order.
    #[must_use]
    pub fn services(&self) -> Vec<ServiceSnapshot> {
        self.store
            .read()
            .services
            .values()
            .map(|entry| entry.snapshot.clone())
            .collect()
    }

    /// Return one mirrored service snapshot.
    #[must_use]
    pub fn service(&self, id: &str) -> Option<ServiceSnapshot> {
        self.store
            .read()
            .services
            .get(id)
            .map(|entry| entry.snapshot.clone())
    }

    /// Return mirrored log records newer than `after` and the cache's retention marker.
    #[must_use]
    pub fn logs_since(&self, id: &str, after: u64) -> (Option<u64>, Vec<LogLine>) {
        self.store.read().services.get(id).map_or_else(
            || (None, Vec::new()),
            |entry| {
                // A cursor at or beyond the newest exposed seq means the caller cached a seq
                // space this mirror no longer serves — the session instance was replaced or the
                // service entry was recreated, restarting server sequences from one. Callers
                // track a strictly lower cursor (the render path re-fetches its newest seq), so
                // this cannot occur within one seq space. Report a cleared history with the full
                // tail so the caller drops its dead cache and repopulates in the same query.
                let regressed = entry.logs.back().is_some_and(|newest| after >= newest.seq);
                if regressed {
                    return (None, entry.logs.iter().cloned().collect());
                }
                (
                    entry.first_retained_seq,
                    entry
                        .logs
                        .iter()
                        .filter(|line| line.seq > after)
                        .cloned()
                        .collect(),
                )
            },
        )
    }

    /// Return retained healthcheck attempts for one mirrored service.
    #[must_use]
    pub fn healthchecks(&self, id: &str) -> Vec<HealthAttempt> {
        self.store
            .read()
            .services
            .get(id)
            .map_or_else(Vec::new, |entry| entry.health.clone())
    }

    /// Return the mirrored lifecycle events of one service, oldest first.
    ///
    /// It is `None` when the session predates [`EVENTS_PROTOCOL_VERSION`] and reports no events.
    #[must_use]
    pub fn events(&self, id: &str) -> Option<Vec<micromux::ServiceEvent>> {
        let store = self.store.read();
        if !accepts_events(&store.session) {
            return None;
        }
        Some(
            store
                .services
                .get(id)
                .map_or_else(Vec::new, |entry| entry.events.events()),
        )
    }

    /// Subscribe to mirror invalidations.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<SessionChange> {
        self.changes.subscribe()
    }

    /// Queue a remote service restart.
    pub fn restart(&self, id: ServiceID) {
        let _ = self.commands.send(RemoteCommand::Restart(id));
    }

    /// Queue a restart of every enabled service.
    pub fn restart_all(&self) {
        let _ = self.commands.send(RemoteCommand::RestartAll);
    }

    /// Queue a remote service enable.
    pub fn enable(&self, id: ServiceID) {
        let _ = self.commands.send(RemoteCommand::Enable(id));
    }

    /// Queue a remote service disable.
    pub fn disable(&self, id: ServiceID) {
        let _ = self.commands.send(RemoteCommand::Disable(id));
    }

    /// Queue retirement of a remote dynamic service.
    pub fn stop_dynamic(&self, id: ServiceID) {
        let _ = self.commands.send(RemoteCommand::StopDynamic(id));
    }

    /// Stop the mirror task without affecting the remote session.
    pub fn cancel(&self) {
        self.shutdown.cancel();
    }
}

#[cfg(test)]
impl RemoteSource {
    /// A mirror of an empty current-protocol session with no background task, for tests that
    /// drive the TUI over it; the forwarding ends show what its terminal sends.
    pub(crate) fn offline() -> (Self, TerminalForwarding) {
        Self::offline_at(micromux_control::PROTOCOL_VERSION)
    }

    /// A mirror of an empty session speaking `protocol_version`, as [`Self::offline`] is.
    pub(crate) fn offline_at(
        protocol_version: micromux_control::ProtocolVersion,
    ) -> (Self, TerminalForwarding) {
        let session = SessionInfo {
            session: None,
            protocol_version,
            id: "offline".to_string(),
            pid: 1,
            start_time: 1,
            name: "offline".to_string(),
            working_dir: "/".to_string(),
            config_path: "/micromux.yaml".to_string(),
            services: Vec::new(),
            services_truncated: false,
            micromux_version: "test".to_string(),
            capabilities: None,
        };
        let (changes, _) = broadcast::channel(CHANGE_CHANNEL_CAPACITY);
        let (commands, _) = mpsc::unbounded_channel();
        let (terminal, forwarding) = TerminalControl::forwarding(MAX_REMOTE_PASTE_BYTES);
        let source = Self {
            store: Arc::new(RwLock::new(MirrorStore {
                session,
                services: IndexMap::new(),
                connected: true,
                notice: None,
            })),
            changes,
            commands,
            terminal,
            shutdown: CancellationToken::new(),
        };
        (source, forwarding)
    }

    /// Replaces the mirrored lifecycle events of service `id` with `events`, as a refresh after an
    /// events change would, and tells the TUI with that change alone.
    pub(crate) fn record_events(&self, id: &str, events: Vec<micromux::ServiceEvent>) {
        if let Some(entry) = self.store.write().services.get_mut(id) {
            entry.events.replace(events);
        }
        publish(
            &self.changes,
            SessionChange {
                service_id: id.to_string(),
                kind: ChangeKind::Events,
            },
        );
    }

    /// Mirrors `snapshot` with `events` as its retained lifecycle events, as a refresh would, and
    /// tells the TUI so.
    pub(crate) fn mirror_service(
        &self,
        snapshot: ServiceSnapshot,
        events: Vec<micromux::ServiceEvent>,
    ) {
        let id = snapshot.id.clone();
        let mut timeline = Timeline::default();
        timeline.replace(events);
        self.store.write().services.insert(
            id.clone(),
            MirrorEntry {
                snapshot,
                logs: VecDeque::new(),
                next_log_seq: 0,
                first_retained_seq: None,
                health: Vec::new(),
                events: timeline,
                clear_logs_after_generation: None,
            },
        );
        publish(
            &self.changes,
            SessionChange {
                service_id: SessionChange::SESSION_WIDE.to_string(),
                kind: ChangeKind::Roster,
            },
        );
        publish(
            &self.changes,
            SessionChange {
                service_id: id,
                kind: ChangeKind::Events,
            },
        );
    }
}

impl Drop for RemoteSource {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn run_remote(
    endpoint: ControlEndpoint,
    store: Arc<RwLock<MirrorStore>>,
    changes: broadcast::Sender<SessionChange>,
    mut inbox: RemoteInbox,
    shutdown: CancellationToken,
    mut request: Client,
    mut subscription: Subscription,
) {
    let mut backoff = ReconnectBackoff::new();
    let mut connected_at = tokio::time::Instant::now();
    loop {
        match run_connected(
            &mut request,
            &mut subscription,
            &store,
            &changes,
            &mut inbox,
            &shutdown,
        )
        .await
        {
            ConnectedLoopExit::Shutdown => return,
            ConnectedLoopExit::Reconnect => mark_disconnected(&store, &changes),
        }
        if connected_at.elapsed() >= STABLE_CONNECTION_DURATION {
            backoff.reset();
        }

        // Refuse input until the session is back, so the TUI drops what is typed meanwhile
        // instead of queueing it behind a full channel and sending it after the reconnect.
        inbox.terminal.set_accepting(false);
        let previous_session = store.read().session.clone();
        loop {
            let delay = backoff.next_delay();
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }

            let established = tokio::select! {
                () = shutdown.cancelled() => return,
                established = establish(&endpoint, &store, &changes) => established,
            };
            match established {
                Ok((new_request, new_subscription)) => {
                    request = new_request;
                    subscription = new_subscription;
                    connected_at = tokio::time::Instant::now();
                    break;
                }
                Err(err) => {
                    tracing::debug!(?err, "remote mirror reconnect failed");
                }
            }
        }

        if resume_input(&mut inbox.terminal) {
            record_notice(
                &store,
                &changes,
                "input typed before the connection broke was discarded".to_string(),
            );
        }
        // A replacement session starts its PTYs at its own default size, so assert ours again,
        // if the TUI still holds one: it withdraws its size when it leaves PTY input mode.
        // The same session still holds the last size it received, which may be another client's.
        if !store.read().session.is_same_instance(&previous_session) {
            inbox.terminal.size.mark_changed();
        }
    }
}

/// Discards input queued before the connection broke, then accepts input again, and returns
/// whether anything was discarded.
///
/// Input typed while disconnected never got queued, because the capability refused it.
fn resume_input(terminal: &mut TerminalForwarding) -> bool {
    let mut discarded = false;
    while let Ok(input) = terminal.input.try_recv() {
        drop(input);
        discarded = true;
    }
    terminal.set_accepting(true);
    discarded
}

async fn run_connected(
    request: &mut Client,
    subscription: &mut Subscription,
    store: &Arc<RwLock<MirrorStore>>,
    changes: &broadcast::Sender<SessionChange>,
    inbox: &mut RemoteInbox,
    shutdown: &CancellationToken,
) -> ConnectedLoopExit {
    // The session's routing status changes without any service event, so poll its description.
    let mut describe = tokio::time::interval(SESSION_REFRESH_INTERVAL);
    describe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    describe.reset();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return ConnectedLoopExit::Shutdown,
            _ = describe.tick() => {
                match request.describe().await {
                    Ok(session) => refresh_summary(store, changes, session),
                    Err(err) if reconnect_after_refresh_error(&err) => {
                        return ConnectedLoopExit::Reconnect;
                    }
                    Err(err) => record_notice(store, changes, err.to_string()),
                }
            }
            command = inbox.commands.recv() => {
                let Some(command) = command else {
                    return ConnectedLoopExit::Shutdown;
                };
                if let Err(err) = execute_command(request, store, changes, command).await {
                    if reconnect_after_refresh_error(&err) {
                        return ConnectedLoopExit::Reconnect;
                    }
                    record_notice(store, changes, err.to_string());
                }
            }
            Some(input) = inbox.terminal.input.recv() => {
                if let Err(err) = forward_input(request, store, changes, input).await {
                    if reconnect_after_refresh_error(&err) {
                        return ConnectedLoopExit::Reconnect;
                    }
                    record_notice(store, changes, err.to_string());
                }
            }
            Ok(()) = inbox.terminal.size.changed() => {
                if let Err(err) =
                    forward_latest_size(request, store, changes, &mut inbox.terminal.size).await
                {
                    if reconnect_after_refresh_error(&err) {
                        return ConnectedLoopExit::Reconnect;
                    }
                    record_notice(store, changes, err.to_string());
                }
            }
            received = subscription.recv() => {
                match received {
                    Ok(Some(change)) => {
                        let mut batch = vec![change];
                        let mut reconnect_after_batch = false;
                        while let Some(received) = subscription.recv().now_or_never() {
                            match received {
                                Ok(Some(change)) => batch.push(change),
                                Ok(None) => {
                                    reconnect_after_batch = true;
                                    break;
                                }
                                Err(err) if reconnect_after_subscription_error(&err) => {
                                    reconnect_after_batch = true;
                                    break;
                                }
                                Err(err) => record_notice(store, changes, err.to_string()),
                            }
                        }
                        let batch = CoalescedChanges::new(batch);
                        if let Err(err) = refresh_and_forward_batch(request, store, changes, &batch).await {
                            if reconnect_after_refresh_error(&err) {
                                return ConnectedLoopExit::Reconnect;
                            }
                            record_notice(store, changes, err.to_string());
                            publish_batch(changes, &batch);
                        }
                        if reconnect_after_batch {
                            return ConnectedLoopExit::Reconnect;
                        }
                    }
                    Ok(None) => return ConnectedLoopExit::Reconnect,
                    Err(err) if reconnect_after_subscription_error(&err) => {
                        return ConnectedLoopExit::Reconnect;
                    }
                    Err(err) => record_notice(store, changes, err.to_string()),
                }
            }
        }
    }
}

async fn establish(
    endpoint: &ControlEndpoint,
    store: &Arc<RwLock<MirrorStore>>,
    changes: &broadcast::Sender<SessionChange>,
) -> Result<(Client, Subscription), ControlError> {
    let mut request = Client::connect(endpoint).await?;
    let session = request.describe().await?;
    update_session(store, session);
    let (subscription, service_ids) =
        subscribe_without_sync_gap(endpoint, &mut request, store).await?;
    store.write().connected = true;
    publish_full_refresh(changes, &service_ids);
    Ok((request, subscription))
}

/// Takes the session's latest URLs and routing status from a fresh description, waking the TUI
/// when they changed.
fn refresh_summary(
    store: &Arc<RwLock<MirrorStore>>,
    changes: &broadcast::Sender<SessionChange>,
    session: SessionInfo,
) {
    {
        let mut guard = store.write();
        if !guard.session.is_same_instance(&session)
            || guard.session.session.as_deref() == session.session.as_deref()
        {
            return;
        }
        guard.session.session = session.session;
    }
    publish(
        changes,
        SessionChange {
            service_id: SessionChange::SESSION_WIDE.to_string(),
            kind: ChangeKind::Status,
        },
    );
}

fn update_session(store: &Arc<RwLock<MirrorStore>>, session: SessionInfo) {
    let mut guard = store.write();
    if !guard.session.is_same_instance(&session) {
        guard.services.clear();
        guard.notice = None;
    }
    guard.session = session;
}

async fn subscribe_without_sync_gap(
    endpoint: &ControlEndpoint,
    request: &mut Client,
    store: &Arc<RwLock<MirrorStore>>,
) -> Result<(Subscription, Vec<ServiceID>), ControlError> {
    // The second sync catches changes that preceded subscription; changes during that sync stay
    // queued for the connected loop, so the handoff cannot lose a transition.
    full_sync(request, store).await?;
    let peer_version = store.read().session.protocol_version;
    let subscription = Client::subscribe_for_version(endpoint, peer_version).await?;
    let service_ids = full_sync(request, store).await?;
    Ok((subscription, service_ids))
}

async fn full_sync<C: RequestConnection>(
    request: &mut C,
    store: &Arc<RwLock<MirrorStore>>,
) -> Result<Vec<ServiceID>, ControlError> {
    let snapshots = request_services(request).await?;
    reconcile_services(store, snapshots);
    let service_ids = store.read().services.keys().cloned().collect::<Vec<_>>();
    for service_id in &service_ids {
        refresh_logs(request, store, service_id).await?;
        refresh_health(request, store, service_id).await?;
        refresh_events(request, store, service_id).await?;
    }
    Ok(store.read().services.keys().cloned().collect())
}

#[cfg(test)]
async fn refresh_and_forward<C: RequestConnection>(
    request: &mut C,
    store: &Arc<RwLock<MirrorStore>>,
    changes: &broadcast::Sender<SessionChange>,
    change: SessionChange,
) -> Result<(), ControlError> {
    refresh_and_forward_batch(
        request,
        store,
        changes,
        &CoalescedChanges::new(vec![change]),
    )
    .await
}

async fn refresh_and_forward_batch<C: RequestConnection>(
    request: &mut C,
    store: &Arc<RwLock<MirrorStore>>,
    changes: &broadcast::Sender<SessionChange>,
    batch: &CoalescedChanges,
) -> Result<(), ControlError> {
    if batch.iter().any(|change| {
        matches!(
            change.kind,
            ChangeKind::Status | ChangeKind::Roster | ChangeKind::Unknown
        )
    }) {
        refresh_roster(request, store).await?;
    }
    for change in batch.iter() {
        match change.kind {
            ChangeKind::Logs => refresh_logs(request, store, &change.service_id).await?,
            ChangeKind::Health => refresh_health(request, store, &change.service_id).await?,
            ChangeKind::Events => refresh_events(request, store, &change.service_id).await?,
            ChangeKind::Status
            | ChangeKind::Roster
            | ChangeKind::Unknown
            | ChangeKind::Heartbeat => {}
        }
    }
    publish_batch(changes, batch);
    Ok(())
}

fn publish_batch(changes: &broadcast::Sender<SessionChange>, batch: &CoalescedChanges) {
    for change in batch.iter().cloned() {
        publish(changes, change);
    }
}

async fn refresh_roster<C: RequestConnection>(
    request: &mut C,
    store: &Arc<RwLock<MirrorStore>>,
) -> Result<(), ControlError> {
    let snapshots = request_services(request).await?;
    let needs_hydration = reconcile_services(store, snapshots);
    for service_id in needs_hydration {
        refresh_logs(request, store, &service_id).await?;
        refresh_health(request, store, &service_id).await?;
        refresh_events(request, store, &service_id).await?;
    }
    Ok(())
}

async fn request_services<C: RequestConnection>(
    request: &mut C,
) -> Result<Vec<ServiceSnapshot>, ControlError> {
    match request.request(Request::ListServices).await? {
        Response::Services(snapshots) => Ok(snapshots),
        other => Err(unexpected_response(&other)),
    }
}

fn reconcile_services(
    store: &Arc<RwLock<MirrorStore>>,
    snapshots: Vec<ServiceSnapshot>,
) -> Vec<ServiceID> {
    let mut guard = store.write();
    let mut old = std::mem::take(&mut guard.services);
    let mut reconciled = IndexMap::with_capacity(snapshots.len());
    let mut needs_hydration = Vec::new();
    for snapshot in snapshots {
        let id = snapshot.id.clone();
        let entry = if let Some(mut entry) = old.shift_remove(&id) {
            if entry
                .clear_logs_after_generation
                .is_some_and(|generation| snapshot.run_generation > generation)
            {
                replace_log_tail(&mut entry, Vec::new());
                entry.clear_logs_after_generation = None;
                needs_hydration.push(id.clone());
            }
            entry.snapshot = snapshot;
            entry
        } else {
            needs_hydration.push(id.clone());
            MirrorEntry {
                snapshot,
                logs: VecDeque::new(),
                next_log_seq: 0,
                first_retained_seq: None,
                health: Vec::new(),
                events: Timeline::default(),
                clear_logs_after_generation: None,
            }
        };
        reconciled.insert(id, entry);
    }
    guard.services = reconciled;
    needs_hydration
}

async fn refresh_logs<C: RequestConnection>(
    request: &mut C,
    store: &Arc<RwLock<MirrorStore>>,
    service_id: &str,
) -> Result<(), ControlError> {
    let Some(cursor) = store
        .read()
        .services
        .get(service_id)
        .map(|entry| entry.next_log_seq)
    else {
        return Ok(());
    };
    let after = (cursor > 0).then(|| cursor.saturating_sub(1));
    let Some((mut lines, mut truncated, first_retained_seq)) =
        request_log_page(request, service_id, after).await?
    else {
        store.write().services.shift_remove(service_id);
        return Ok(());
    };

    let must_retail = cursor > 0
        && first_retained_seq.is_some_and(|first| {
            first > cursor.saturating_add(1) || (lines.is_empty() && first > cursor)
        });
    if must_retail {
        let Some((tail, _, _)) = request_log_page(request, service_id, None).await? else {
            store.write().services.shift_remove(service_id);
            return Ok(());
        };
        if let Some(entry) = store.write().services.get_mut(service_id) {
            replace_log_tail(entry, tail);
        }
        return Ok(());
    }

    loop {
        if let Some(entry) = store.write().services.get_mut(service_id) {
            append_log_page(entry, lines);
        } else {
            return Ok(());
        }
        if !truncated {
            return Ok(());
        }
        let next = store
            .read()
            .services
            .get(service_id)
            .map_or(0, |entry| entry.next_log_seq);
        let Some((next_lines, next_truncated, _)) =
            request_log_page(request, service_id, Some(next)).await?
        else {
            store.write().services.shift_remove(service_id);
            return Ok(());
        };
        if next_lines.is_empty() && next_truncated {
            return Err(ControlError::Unexpected(
                "truncated FollowLogs response did not advance its cursor".to_string(),
            ));
        }
        lines = next_lines;
        truncated = next_truncated;
    }
}

async fn request_log_page<C: RequestConnection>(
    request: &mut C,
    service_id: &str,
    after: Option<u64>,
) -> Result<Option<(Vec<LogLine>, bool, Option<u64>)>, ControlError> {
    match request
        .request(Request::FollowLogs {
            service: service_id.to_string(),
            run_generation: None,
            after,
        })
        .await?
    {
        Response::Logs {
            lines,
            truncated,
            first_retained_seq,
        } => Ok(Some((lines, truncated, first_retained_seq))),
        Response::Error {
            code: ErrorCode::UnknownService,
            ..
        } => Ok(None),
        other => Err(unexpected_response(&other)),
    }
}

fn append_log_page(entry: &mut MirrorEntry, lines: Vec<LogLine>) {
    for line in lines {
        if entry
            .logs
            .back()
            .is_some_and(|current| current.seq == line.seq)
        {
            entry.logs.pop_back();
            entry.logs.push_back(line);
        } else if line.seq > entry.next_log_seq {
            entry.logs.push_back(line);
        }
        if let Some(last) = entry.logs.back() {
            entry.next_log_seq = entry.next_log_seq.max(last.seq);
        }
    }
    while entry.logs.len() > REMOTE_LOG_TAIL {
        entry.logs.pop_front();
    }
    entry.first_retained_seq = entry.logs.front().map(|line| line.seq);
}

fn replace_log_tail(entry: &mut MirrorEntry, lines: Vec<LogLine>) {
    entry.logs.clear();
    entry.next_log_seq = 0;
    entry.first_retained_seq = None;
    append_log_page(entry, lines);
}

async fn refresh_health<C: RequestConnection>(
    request: &mut C,
    store: &Arc<RwLock<MirrorStore>>,
    service_id: &str,
) -> Result<(), ControlError> {
    match request
        .request(Request::GetHealthHistory {
            service: service_id.to_string(),
        })
        .await?
    {
        Response::HealthHistory { attempts } => {
            if let Some(entry) = store.write().services.get_mut(service_id) {
                entry.health = attempts;
            }
        }
        Response::Error {
            code: ErrorCode::UnknownService,
            ..
        } => {
            store.write().services.shift_remove(service_id);
        }
        other => return Err(unexpected_response(&other)),
    }
    Ok(())
}

/// Brings the mirror's copy of a service's lifecycle events up to date, reading only events newer
/// than the newest one it holds.
async fn refresh_events<C: RequestConnection>(
    request: &mut C,
    store: &Arc<RwLock<MirrorStore>>,
    service_id: &str,
) -> Result<(), ControlError> {
    if !accepts_events(&store.read().session) {
        return Ok(());
    }
    loop {
        let Some(after) = store
            .read()
            .services
            .get(service_id)
            .map(|entry| entry.events.read_after())
        else {
            return Ok(());
        };
        // An empty copy takes the retained tail in one page; it needs no continuation.
        let Some(after) = after else {
            return replace_events(request, store, service_id).await;
        };
        let Some(EventPage { events, truncated }) =
            request_events(request, service_id, Some(after)).await?
        else {
            store.write().services.shift_remove(service_id);
            return Ok(());
        };
        let extended = store.write().services.get_mut(service_id).map(|entry| {
            entry
                .events
                .extend(events)
                .then(|| entry.events.read_after())
        });
        let next = match extended {
            None => return Ok(()),
            Some(None) => return replace_events(request, store, service_id).await,
            Some(Some(next)) => next,
        };
        // A page the response size bound cut short continues from its own newest event.
        if !truncated {
            return Ok(());
        }
        if next <= Some(after) {
            return Err(ControlError::Unexpected(
                "truncated GetEvents response did not advance its cursor".to_string(),
            ));
        }
    }
}

/// Replaces the mirror's copy of a service's lifecycle events with the session's retained tail.
async fn replace_events<C: RequestConnection>(
    request: &mut C,
    store: &Arc<RwLock<MirrorStore>>,
    service_id: &str,
) -> Result<(), ControlError> {
    let Some(EventPage { events, .. }) = request_events(request, service_id, None).await? else {
        store.write().services.shift_remove(service_id);
        return Ok(());
    };
    if let Some(entry) = store.write().services.get_mut(service_id) {
        entry.events.replace(events);
    }
    Ok(())
}

/// One `GetEvents` reply.
struct EventPage {
    events: Vec<micromux::ServiceEvent>,
    /// Whether retention or the response size bound left out matching events.
    truncated: bool,
}

/// Reads one page of a service's lifecycle events, or `None` once the session no longer knows
/// the service.
async fn request_events<C: RequestConnection>(
    request: &mut C,
    service_id: &str,
    after: Option<u64>,
) -> Result<Option<EventPage>, ControlError> {
    match request
        .request(Request::GetEvents {
            service: service_id.to_string(),
            after,
            tail: Some(micromux::EVENT_HISTORY),
        })
        .await?
    {
        Response::Events { events, truncated } => Ok(Some(EventPage { events, truncated })),
        Response::Error {
            code: ErrorCode::UnknownService,
            ..
        } => Ok(None),
        other => Err(unexpected_response(&other)),
    }
}

async fn execute_command(
    request: &mut Client,
    store: &Arc<RwLock<MirrorStore>>,
    changes: &broadcast::Sender<SessionChange>,
    command: RemoteCommand,
) -> Result<(), ControlError> {
    let (request_message, expected_response) = match command {
        RemoteCommand::Restart(service) => (
            Request::Restart { service },
            ExpectedCommandResponse::RestartAccepted,
        ),
        RemoteCommand::RestartAll => (
            Request::RestartAll,
            ExpectedCommandResponse::RestartAccepted,
        ),
        RemoteCommand::Enable(service) => (
            Request::Enable { service },
            ExpectedCommandResponse::Accepted,
        ),
        RemoteCommand::Disable(service) => (
            Request::Disable { service },
            ExpectedCommandResponse::Accepted,
        ),
        RemoteCommand::StopDynamic(service) => (
            Request::StopDynamicService { service },
            ExpectedCommandResponse::Dynamic,
        ),
    };
    let response = request.request(request_message).await?;
    let notice = match (response, expected_response) {
        (Response::Accepted { services }, ExpectedCommandResponse::RestartAccepted) => {
            mark_pending_log_clears(store, &services);
            None
        }
        (Response::Accepted { .. }, ExpectedCommandResponse::Accepted)
        | (Response::DynamicService(_), ExpectedCommandResponse::Dynamic) => None,
        (Response::Error { message, .. }, _) => Some(message),
        (other, _) => Some(format!("unexpected control response: {other:?}")),
    };
    store.write().notice = notice;
    publish(
        changes,
        SessionChange {
            service_id: SessionChange::SESSION_WIDE.to_string(),
            kind: ChangeKind::Status,
        },
    );
    Ok(())
}

fn accepts_events(session: &SessionInfo) -> bool {
    session.protocol_version.supports(EVENTS_PROTOCOL_VERSION)
}

fn accepts_terminal(session: &SessionInfo) -> bool {
    session.protocol_version.supports(TERMINAL_PROTOCOL_VERSION)
}

/// Sends one prepared input to the session, keeping its byte reservation until the session has
/// queued it, so the forwarding budgets also bound input in flight.
async fn forward_input<C: RequestConnection>(
    request: &mut C,
    store: &Arc<RwLock<MirrorStore>>,
    changes: &broadcast::Sender<SessionChange>,
    input: PreparedPtyInput,
) -> Result<(), ControlError> {
    // A reconnect may land on an older session than the one the TUI checked at startup.
    if !accepts_terminal(&store.read().session) {
        record_notice(
            store,
            changes,
            "input discarded: this session does not accept terminal input".to_string(),
        );
        return Ok(());
    }
    let response = request
        .request(Request::SendInput {
            service: input.service_id().to_string(),
            data: InputBytes::from(input.data().to_vec()),
            paste: input.kind() == PtyInputKind::Paste,
        })
        .await?;
    drop(input);
    settle_terminal_response(store, changes, response);
    Ok(())
}

/// Forwards the latest requested size, if any.
///
/// When the connection breaks first, the size stays marked as changed, so the next connection
/// sends it even to the same session, which never received it.
async fn forward_latest_size<C: RequestConnection>(
    request: &mut C,
    store: &Arc<RwLock<MirrorStore>>,
    changes: &broadcast::Sender<SessionChange>,
    size: &mut tokio::sync::watch::Receiver<Option<TerminalSize>>,
) -> Result<(), ControlError> {
    let Some(latest) = *size.borrow_and_update() else {
        return Ok(());
    };
    let result = forward_resize(request, store, changes, latest).await;
    if let Err(err) = &result
        && reconnect_after_refresh_error(err)
    {
        size.mark_changed();
    }
    result
}

async fn forward_resize<C: RequestConnection>(
    request: &mut C,
    store: &Arc<RwLock<MirrorStore>>,
    changes: &broadcast::Sender<SessionChange>,
    size: TerminalSize,
) -> Result<(), ControlError> {
    if !accepts_terminal(&store.read().session) {
        return Ok(());
    }
    let TerminalSize { cols, rows } = size;
    let response = request
        .request(Request::ResizeTerminal { cols, rows })
        .await?;
    settle_terminal_response(store, changes, response);
    Ok(())
}

/// Shows a refused input or resize in the header, and clears the notice once one succeeds again.
///
/// A success publishes nothing while there is no notice, so keystrokes do not wake the TUI for
/// nothing.
fn settle_terminal_response(
    store: &Arc<RwLock<MirrorStore>>,
    changes: &broadcast::Sender<SessionChange>,
    response: Response,
) {
    let notice = match response {
        Response::TerminalQueued => None,
        Response::Error { message, .. } => Some(message),
        other => Some(format!("unexpected control response: {other:?}")),
    };
    {
        let mut guard = store.write();
        if notice.is_none() && guard.notice.is_none() {
            return;
        }
        guard.notice = notice;
    }
    publish(
        changes,
        SessionChange {
            service_id: SessionChange::SESSION_WIDE.to_string(),
            kind: ChangeKind::Status,
        },
    );
}

fn mark_pending_log_clears(store: &Arc<RwLock<MirrorStore>>, services: &[ServiceCommandAck]) {
    let mut guard = store.write();
    for acknowledgement in services {
        if let Some(entry) = guard.services.get_mut(&acknowledgement.service) {
            entry.clear_logs_after_generation = Some(
                entry
                    .clear_logs_after_generation
                    .map_or(acknowledgement.observed_generation, |generation| {
                        generation.max(acknowledgement.observed_generation)
                    }),
            );
        }
    }
}

fn unexpected_response(response: &Response) -> ControlError {
    ControlError::Unexpected(format!("{response:?}"))
}

fn reconnect_after_refresh_error(error: &ControlError) -> bool {
    matches!(
        error,
        ControlError::Io(_) | ControlError::Closed | ControlError::Timeout
    )
}

fn reconnect_after_subscription_error(error: &ControlError) -> bool {
    matches!(
        error,
        ControlError::Io(_) | ControlError::Closed | ControlError::Timeout
    )
}

fn mark_disconnected(store: &Arc<RwLock<MirrorStore>>, changes: &broadcast::Sender<SessionChange>) {
    store.write().connected = false;
    publish(
        changes,
        SessionChange {
            service_id: SessionChange::SESSION_WIDE.to_string(),
            kind: ChangeKind::Roster,
        },
    );
}

fn record_notice(
    store: &Arc<RwLock<MirrorStore>>,
    changes: &broadcast::Sender<SessionChange>,
    notice: String,
) {
    store.write().notice = Some(notice);
    publish(
        changes,
        SessionChange {
            service_id: SessionChange::SESSION_WIDE.to_string(),
            kind: ChangeKind::Status,
        },
    );
}

fn publish_full_refresh(changes: &broadcast::Sender<SessionChange>, service_ids: &[ServiceID]) {
    publish(
        changes,
        SessionChange {
            service_id: SessionChange::SESSION_WIDE.to_string(),
            kind: ChangeKind::Roster,
        },
    );
    for service_id in service_ids {
        for kind in [
            ChangeKind::Status,
            ChangeKind::Logs,
            ChangeKind::Health,
            ChangeKind::Events,
        ] {
            publish(
                changes,
                SessionChange {
                    service_id: service_id.clone(),
                    kind,
                },
            );
        }
    }
}

fn publish(changes: &broadcast::Sender<SessionChange>, change: SessionChange) {
    let _ = changes.send(change);
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use color_eyre::eyre;
    use micromux::RestartPolicy;
    use similar_asserts::assert_eq;

    struct ScriptedConnection {
        responses: VecDeque<Response>,
        requests: Vec<Request>,
    }

    impl ScriptedConnection {
        fn new(responses: impl IntoIterator<Item = Response>) -> Self {
            Self {
                responses: responses.into_iter().collect(),
                requests: Vec::new(),
            }
        }
    }

    impl RequestConnection for ScriptedConnection {
        #[expect(
            clippy::unused_async_trait_impl,
            reason = "the scripted double mirrors the async trait and, like the real client, records \
                      the request only when polled"
        )]
        async fn request(&mut self, request: Request) -> Result<Response, ControlError> {
            self.requests.push(request);
            self.responses.pop_front().ok_or_else(|| {
                ControlError::Unexpected("scripted connection exhausted".to_string())
            })
        }
    }

    fn session() -> SessionInfo {
        SessionInfo {
            session: None,
            protocol_version: micromux_control::PROTOCOL_VERSION,
            id: "session".to_string(),
            pid: 1,
            start_time: 1,
            name: "test".to_string(),
            working_dir: "/tmp".to_string(),
            config_path: "/tmp/micromux.yaml".to_string(),
            services: Vec::new(),
            services_truncated: false,
            micromux_version: "test".to_string(),
            capabilities: None,
        }
    }

    fn snapshot(id: &str, run_generation: u64) -> ServiceSnapshot {
        let mut snapshot = ServiceSnapshot::initial(
            id.to_string(),
            id.to_string(),
            Vec::new(),
            None,
            RestartPolicy::Never,
            vec!["true".to_string()],
            None,
        );
        snapshot.run_generation = run_generation;
        snapshot
    }

    fn line(seq: u64, text: &str) -> LogLine {
        LogLine {
            seq,
            run_generation: 1,
            timestamp_unix_ms: seq,
            line: text.to_string(),
        }
    }

    #[test]
    fn reconnect_backoff_survives_short_lived_reconnections() {
        let mut backoff = ReconnectBackoff::new();

        assert_eq!(backoff.next_delay(), Duration::from_millis(250));
        assert_eq!(backoff.next_delay(), Duration::from_millis(500));
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(250));
    }

    #[test]
    fn subscription_timeouts_trigger_reconnection() {
        assert!(reconnect_after_subscription_error(&ControlError::Timeout));
    }

    #[tokio::test]
    async fn ready_change_batches_coalesce_remote_round_trips() -> eyre::Result<()> {
        let store = store_with(entry_with_log(1, "line"));
        let mut request = ScriptedConnection::new([
            Response::Services(vec![snapshot("svc", 1)]),
            Response::Logs {
                lines: vec![line(2, "new")],
                truncated: false,
                first_retained_seq: Some(1),
            },
        ]);
        let (changes, _) = broadcast::channel(8);
        let batch = CoalescedChanges::new(vec![
            SessionChange {
                service_id: "svc".to_string(),
                kind: ChangeKind::Status,
            },
            SessionChange {
                service_id: "svc".to_string(),
                kind: ChangeKind::Status,
            },
            SessionChange {
                service_id: "other".to_string(),
                kind: ChangeKind::Status,
            },
            SessionChange {
                service_id: "svc".to_string(),
                kind: ChangeKind::Logs,
            },
            SessionChange {
                service_id: "svc".to_string(),
                kind: ChangeKind::Logs,
            },
        ]);

        refresh_and_forward_batch(&mut request, &store, &changes, &batch).await?;

        assert_eq!(request.requests.len(), 2);
        assert_matches!(
            request.requests.as_slice(),
            [Request::ListServices, Request::FollowLogs { .. }]
        );
        Ok(())
    }

    fn store_with(entry: MirrorEntry) -> Arc<RwLock<MirrorStore>> {
        Arc::new(RwLock::new(MirrorStore {
            session: session(),
            services: [(entry.snapshot.id.clone(), entry)].into_iter().collect(),
            connected: true,
            notice: None,
        }))
    }

    fn entry_with_log(seq: u64, text: &str) -> MirrorEntry {
        MirrorEntry {
            snapshot: snapshot("svc", 1),
            logs: [line(seq, text)].into_iter().collect(),
            next_log_seq: seq,
            first_retained_seq: Some(seq),
            health: Vec::new(),
            events: Timeline::default(),
            clear_logs_after_generation: None,
        }
    }

    fn source_for(store: Arc<RwLock<MirrorStore>>) -> RemoteSource {
        let (changes, _) = broadcast::channel(8);
        let (commands, _) = mpsc::unbounded_channel();
        let (terminal, _) = TerminalControl::forwarding(MAX_REMOTE_PASTE_BYTES);
        RemoteSource {
            store,
            changes,
            commands,
            terminal,
            shutdown: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn discontinuity_clears_the_cache_and_retails() -> eyre::Result<()> {
        let store = store_with(entry_with_log(10, "old"));
        let mut request = ScriptedConnection::new([
            Response::Logs {
                lines: vec![line(15, "first retained"), line(16, "incremental")],
                truncated: false,
                first_retained_seq: Some(15),
            },
            Response::Logs {
                lines: vec![line(20, "tail"), line(21, "latest")],
                truncated: false,
                first_retained_seq: Some(20),
            },
        ]);

        refresh_logs(&mut request, &store, "svc").await?;

        let source = source_for(store);
        let (first_retained, lines) = source.logs_since("svc", 10);
        assert_eq!(first_retained, Some(20));
        assert_eq!(
            lines
                .iter()
                .map(|line| (line.seq, line.line.as_str()))
                .collect::<Vec<_>>(),
            vec![(20, "tail"), (21, "latest")]
        );
        assert!(matches!(
            request.requests.get(1),
            Some(Request::FollowLogs { after: None, .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn empty_incremental_page_clears_the_cache_and_retails() -> eyre::Result<()> {
        let store = store_with(entry_with_log(10, "old incarnation"));
        let mut request = ScriptedConnection::new([
            Response::Logs {
                lines: Vec::new(),
                truncated: false,
                first_retained_seq: Some(11),
            },
            Response::Logs {
                lines: vec![line(11, "new line")],
                truncated: false,
                first_retained_seq: Some(11),
            },
        ]);

        refresh_logs(&mut request, &store, "svc").await?;

        let source = source_for(store);
        let (first_retained, lines) = source.logs_since("svc", 0);
        assert_eq!(first_retained, Some(11));
        assert_eq!(
            lines
                .iter()
                .map(|line| (line.seq, line.line.as_str()))
                .collect::<Vec<_>>(),
            vec![(11, "new line")]
        );
        assert!(matches!(
            request.requests.as_slice(),
            [
                Request::FollowLogs { after: Some(9), .. },
                Request::FollowLogs { after: None, .. }
            ]
        ));
        Ok(())
    }

    #[test]
    fn stale_cursor_beyond_the_exposed_seq_space_reports_a_cleared_history() {
        let source = source_for(store_with(entry_with_log(3, "fresh instance")));

        let (first_retained, lines) = source.logs_since("svc", 59);
        assert_eq!(first_retained, None);
        assert_eq!(
            lines
                .iter()
                .map(|line| (line.seq, line.line.as_str()))
                .collect::<Vec<_>>(),
            vec![(3, "fresh instance")]
        );

        let (first_retained, lines) = source.logs_since("svc", 3);
        assert_eq!(first_retained, None);
        assert_eq!(lines.len(), 1);

        let (first_retained, lines) = source.logs_since("svc", 2);
        assert_eq!(first_retained, Some(3));
        assert_eq!(lines.len(), 1);
    }

    /// A periodic description updates the attached view's routing status and wakes the TUI, but
    /// only when it changed and only for the same session instance.
    #[test]
    fn refreshed_descriptions_update_the_session_summary() {
        let store = store_with(entry_with_log(1, "line"));
        let (changes, mut received) = broadcast::channel(8);
        let summary = |routing| micromux::SessionSummary {
            git: None,
            name: "one".to_string(),
            domain: "one.localhost".to_string(),
            host_suffix: "one.localhost".to_string(),
            proxy_port: 80,
            routes: Vec::new(),
            allocated_ports: IndexMap::new(),
            routing: Some(routing),
        };
        let described = |routing| SessionInfo {
            session: Some(Box::new(summary(routing))),
            ..session()
        };

        refresh_summary(
            &store,
            &changes,
            described(micromux::RoutingStatus::Registered),
        );
        assert!(received.try_recv().is_ok());
        refresh_summary(
            &store,
            &changes,
            described(micromux::RoutingStatus::Registered),
        );
        assert!(received.try_recv().is_err());

        let unavailable = micromux::RoutingStatus::Unavailable {
            reason: "gone".to_string(),
        };
        refresh_summary(&store, &changes, described(unavailable.clone()));
        assert_eq!(
            store
                .read()
                .session
                .session
                .as_ref()
                .and_then(|summary| summary.routing.clone()),
            Some(unavailable)
        );

        // Another instance's description waits for the reconnect path to replace the mirror.
        let mut other = described(micromux::RoutingStatus::Registered);
        other.pid = 2;
        refresh_summary(&store, &changes, other);
        assert!(received.try_recv().is_ok());
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn same_sequence_snapshot_reaches_logs_since_through_the_mirror() -> eyre::Result<()> {
        let store = store_with(entry_with_log(7, "frame one"));
        let mut request = ScriptedConnection::new([Response::Logs {
            lines: vec![line(7, "frame two")],
            truncated: false,
            first_retained_seq: Some(7),
        }]);

        refresh_logs(&mut request, &store, "svc").await?;

        let source = source_for(store);
        let (_, lines) = source.logs_since("svc", 6);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines.first().map(|line| line.line.as_str()),
            Some("frame two")
        );
        assert!(matches!(
            request.requests.first(),
            Some(Request::FollowLogs { after: Some(6), .. })
        ));
        Ok(())
    }

    /// Forwarded input names its service and kind, and a refusal shows in the header until the
    /// next input succeeds.
    #[tokio::test]
    async fn forwarded_input_reports_refusals_until_input_succeeds() -> eyre::Result<()> {
        let store = store_with(entry_with_log(1, "line"));
        let (changes, mut received) = broadcast::channel(8);
        let (terminal, _forwarding) = TerminalControl::forwarding(MAX_REMOTE_PASTE_BYTES);
        let mut request = ScriptedConnection::new([
            Response::error(ErrorCode::LimitExceeded, "backlog is full"),
            Response::TerminalQueued,
            Response::TerminalQueued,
        ]);

        let paste = terminal.prepare_paste("svc".to_string(), b"pasted".to_vec())?;
        forward_input(&mut request, &store, &changes, paste).await?;
        assert_eq!(store.read().notice.as_deref(), Some("backlog is full"));
        assert_eq!(received.try_recv()?.kind, ChangeKind::Status);

        let key = terminal.prepare_input("svc".to_string(), b"k".to_vec())?;
        forward_input(&mut request, &store, &changes, key).await?;
        assert_eq!(store.read().notice, None);
        assert_eq!(received.try_recv()?.kind, ChangeKind::Status);

        // With no notice to clear, a success does not wake the TUI.
        let key = terminal.prepare_input("svc".to_string(), b"j".to_vec())?;
        forward_input(&mut request, &store, &changes, key).await?;
        assert_matches!(
            received.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        );

        assert_matches!(
            request.requests.as_slice(),
            [
                Request::SendInput { service: first, data: pasted, paste: true },
                Request::SendInput { paste: false, .. },
                Request::SendInput { data: last, paste: false, .. },
            ] if first == "svc" && pasted.as_bytes() == b"pasted" && last.as_bytes() == b"j"
        );
        Ok(())
    }

    /// A session older than the terminal protocol never sees input or resizes, which it would
    /// reject as malformed and close the mirror's connection over.
    #[tokio::test]
    async fn older_sessions_never_receive_terminal_requests() -> eyre::Result<()> {
        let store = store_with(entry_with_log(1, "line"));
        store.write().session.protocol_version = micromux_control::ProtocolVersion::new(3, 17);
        let (changes, _received) = broadcast::channel(8);
        let (terminal, _forwarding) = TerminalControl::forwarding(MAX_REMOTE_PASTE_BYTES);
        let source = source_for(Arc::clone(&store));
        assert!(source.terminal().is_none());
        let mut request = ScriptedConnection::new([]);

        let key = terminal.prepare_input("svc".to_string(), b"k".to_vec())?;
        forward_input(&mut request, &store, &changes, key).await?;
        forward_resize(
            &mut request,
            &store,
            &changes,
            TerminalSize { cols: 80, rows: 24 },
        )
        .await?;

        assert!(request.requests.is_empty());
        assert!(
            store
                .read()
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("does not accept terminal input"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn forwarded_resize_carries_the_latest_size() -> eyre::Result<()> {
        let store = store_with(entry_with_log(1, "line"));
        let (changes, _received) = broadcast::channel(8);
        let source = source_for(Arc::clone(&store));
        assert!(source.terminal().is_some());
        let mut request = ScriptedConnection::new([Response::TerminalQueued]);

        forward_resize(
            &mut request,
            &store,
            &changes,
            TerminalSize {
                cols: 132,
                rows: 43,
            },
        )
        .await?;

        assert_matches!(
            request.requests.as_slice(),
            [Request::ResizeTerminal {
                cols: 132,
                rows: 43
            }]
        );
        assert_eq!(store.read().notice, None);
        Ok(())
    }

    /// Reconnecting drops input queued before the connection broke, and the capability refused
    /// input typed meanwhile, so nothing typed during the outage reaches the session.
    #[test]
    fn resuming_after_a_reconnect_discards_stale_input() -> eyre::Result<()> {
        let (terminal, mut forwarding) = TerminalControl::forwarding(MAX_REMOTE_PASTE_BYTES);
        terminal.try_send(terminal.prepare_input("svc".to_string(), b"stale".to_vec())?)?;
        forwarding.set_accepting(false);
        assert_matches!(
            terminal.try_send(terminal.prepare_input("svc".to_string(), b"typed".to_vec())?),
            Err(micromux::PtyInputSendError::Disconnected(_))
        );

        assert!(resume_input(&mut forwarding));
        assert_matches!(
            forwarding.input.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        );

        terminal.try_send(terminal.prepare_input("svc".to_string(), b"fresh".to_vec())?)?;
        assert_eq!(forwarding.input.try_recv()?.data(), b"fresh");
        assert!(!resume_input(&mut forwarding));
        Ok(())
    }

    /// A connection that never answers.
    struct ClosedConnection;

    impl RequestConnection for ClosedConnection {
        #[expect(
            clippy::unused_async_trait_impl,
            reason = "the double mirrors the async trait it implements"
        )]
        async fn request(&mut self, _request: Request) -> Result<Response, ControlError> {
            Err(ControlError::Closed)
        }
    }

    /// A resize lost with the connection is sent again after the reconnect, even to the same
    /// session, which never received it.
    #[tokio::test]
    async fn a_resize_lost_with_the_connection_stays_pending() -> eyre::Result<()> {
        let store = store_with(entry_with_log(1, "line"));
        let (changes, _received) = broadcast::channel(8);
        let (terminal, mut forwarding) = TerminalControl::forwarding(MAX_REMOTE_PASTE_BYTES);
        assert!(terminal.try_resize(100, 30));

        let result = forward_latest_size(
            &mut ClosedConnection,
            &store,
            &changes,
            &mut forwarding.size,
        )
        .await;
        assert_matches!(result, Err(ControlError::Closed));
        assert!(forwarding.size.has_changed()?);

        let mut request = ScriptedConnection::new([Response::TerminalQueued]);
        forward_latest_size(&mut request, &store, &changes, &mut forwarding.size).await?;
        assert!(!forwarding.size.has_changed()?);

        // A withdrawn size is not re-sent, even when a reconnect marks it changed.
        terminal.withdraw_size();
        forwarding.size.mark_changed();
        forward_latest_size(&mut request, &store, &changes, &mut forwarding.size).await?;
        assert_matches!(
            request.requests.as_slice(),
            [Request::ResizeTerminal {
                cols: 100,
                rows: 30
            }]
        );
        Ok(())
    }

    #[tokio::test]
    async fn refresh_stores_before_it_notifies() -> eyre::Result<()> {
        let store = store_with(entry_with_log(1, "line"));
        let mut request = ScriptedConnection::new([Response::Services(vec![snapshot("svc", 2)])]);
        let (changes, _) = broadcast::channel(8);
        let mut received = changes.subscribe();
        let change = SessionChange {
            service_id: "svc".to_string(),
            kind: ChangeKind::Status,
        };

        refresh_and_forward(&mut request, &store, &changes, change).await?;

        let forwarded = received.recv().await?;
        assert_eq!(forwarded.kind, ChangeKind::Status);
        assert_eq!(
            store
                .read()
                .services
                .get("svc")
                .map(|entry| entry.snapshot.run_generation),
            Some(2)
        );
        assert_eq!(store.read().services["svc"].logs.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn subscription_heartbeat_does_not_refresh_the_mirror() -> eyre::Result<()> {
        let store = store_with(entry_with_log(1, "line"));
        let mut request = ScriptedConnection::new([]);
        let (changes, _) = broadcast::channel(8);
        let mut received = changes.subscribe();
        let heartbeat = SessionChange {
            service_id: SessionChange::SESSION_WIDE.to_string(),
            kind: ChangeKind::Heartbeat,
        };

        refresh_and_forward(&mut request, &store, &changes, heartbeat).await?;

        assert!(request.requests.is_empty());
        let forwarded = received.recv().await?;
        assert_eq!(forwarded.service_id, SessionChange::SESSION_WIDE);
        assert_eq!(forwarded.kind, ChangeKind::Heartbeat);
        Ok(())
    }

    #[tokio::test]
    async fn session_wide_roster_change_reconciles_without_service_lookup() -> eyre::Result<()> {
        let store = store_with(entry_with_log(1, "line"));
        let mut request = ScriptedConnection::new([Response::Services(Vec::new())]);
        let (changes, _) = broadcast::channel(8);
        refresh_and_forward(
            &mut request,
            &store,
            &changes,
            SessionChange {
                service_id: SessionChange::SESSION_WIDE.to_string(),
                kind: ChangeKind::Roster,
            },
        )
        .await?;

        assert!(store.read().services.is_empty());
        Ok(())
    }

    fn lifecycle_event(seq: u64) -> micromux::ServiceEvent {
        micromux::ServiceEvent {
            seq,
            at_unix_ms: seq,
            run_generation: 1,
            kind: micromux::ServiceEventKind::Spawned,
            detail: format!("event {seq}"),
            exit_code: None,
            pid: None,
            delay_ms: None,
            blocked_on: None,
        }
    }

    fn lifecycle_events(seqs: std::ops::RangeInclusive<u64>) -> Vec<micromux::ServiceEvent> {
        seqs.map(lifecycle_event).collect()
    }

    fn mirrored_event_seqs(store: &Arc<RwLock<MirrorStore>>) -> Vec<u64> {
        source_for(Arc::clone(store))
            .events("svc")
            .unwrap_or_default()
            .iter()
            .map(|event| event.seq)
            .collect()
    }

    /// The cursor of each request, all of which must be `GetEvents`.
    fn events_cursors(requests: &[Request]) -> eyre::Result<Vec<Option<u64>>> {
        requests
            .iter()
            .map(|request| match request {
                Request::GetEvents { after, .. } => Ok(*after),
                other => Err(eyre::eyre!("expected GetEvents, got {other:?}")),
            })
            .collect()
    }

    /// The mirror reads a service's events forward from the newest it holds, re-reading that
    /// one to prove the session's history still continues its copy.
    #[tokio::test]
    async fn events_refresh_reads_forward_from_the_newest_event_held() -> eyre::Result<()> {
        let store = store_with(entry_with_log(1, "line"));
        let mut request = ScriptedConnection::new([
            Response::Events {
                events: lifecycle_events(1..=2),
                truncated: false,
            },
            Response::Events {
                events: lifecycle_events(2..=4),
                truncated: false,
            },
        ]);

        // An empty copy takes the retained tail; the next refresh only what follows it.
        refresh_events(&mut request, &store, "svc").await?;
        refresh_events(&mut request, &store, "svc").await?;

        assert_eq!(mirrored_event_seqs(&store), vec![1, 2, 3, 4]);
        assert_eq!(events_cursors(&request.requests)?, vec![None, Some(1)]);
        Ok(())
    }

    /// A history that no longer starts with the newest event held replaces the copy with the
    /// retained tail: the service was recreated under its id and numbers its events from one
    /// again, or retention dropped events the mirror never read.
    #[tokio::test]
    async fn events_refresh_retails_after_a_discontinuity() -> eyre::Result<()> {
        // The recreated service recorded six events of its own; the session answers the cursor
        // after event 4 with its events 5 and 6, which differ from the old run's event 5.
        let recreated = |seqs: std::ops::RangeInclusive<u64>| {
            seqs.map(|seq| micromux::ServiceEvent {
                at_unix_ms: 1_000 + seq,
                detail: format!("recreated {seq}"),
                ..lifecycle_event(seq)
            })
            .collect::<Vec<_>>()
        };
        let store = store_with(entry_with_log(1, "line"));
        if let Some(entry) = store.write().services.get_mut("svc") {
            entry.events.replace(lifecycle_events(1..=5));
        }
        let mut request = ScriptedConnection::new([
            Response::Events {
                events: recreated(5..=6),
                truncated: false,
            },
            Response::Events {
                events: recreated(1..=6),
                truncated: false,
            },
        ]);
        refresh_events(&mut request, &store, "svc").await?;
        assert_eq!(
            source_for(Arc::clone(&store)).events("svc"),
            Some(recreated(1..=6))
        );
        assert_eq!(events_cursors(&request.requests)?, vec![Some(4), None]);

        // Retention moved past event 5 before this refresh, so the oldest event left is 9, and
        // the session flags both pages as missing older events.
        let store = store_with(entry_with_log(1, "line"));
        if let Some(entry) = store.write().services.get_mut("svc") {
            entry.events.replace(lifecycle_events(1..=5));
        }
        let mut request = ScriptedConnection::new([
            Response::Events {
                events: lifecycle_events(9..=10),
                truncated: true,
            },
            Response::Events {
                events: lifecycle_events(9..=10),
                truncated: true,
            },
        ]);
        refresh_events(&mut request, &store, "svc").await?;
        assert_eq!(mirrored_event_seqs(&store), vec![9, 10]);
        assert_eq!(events_cursors(&request.requests)?, vec![Some(4), None]);
        Ok(())
    }

    /// A page the response size bound cut short is followed by the next one, from its own newest
    /// event, and a page that cannot advance is an error rather than a loop.
    #[tokio::test]
    async fn events_refresh_follows_truncated_pages() -> eyre::Result<()> {
        let store = store_with(entry_with_log(1, "line"));
        if let Some(entry) = store.write().services.get_mut("svc") {
            entry.events.replace(lifecycle_events(1..=1));
        }
        let mut request = ScriptedConnection::new([
            Response::Events {
                events: lifecycle_events(1..=3),
                truncated: true,
            },
            Response::Events {
                events: lifecycle_events(3..=4),
                truncated: false,
            },
        ]);
        refresh_events(&mut request, &store, "svc").await?;
        assert_eq!(mirrored_event_seqs(&store), vec![1, 2, 3, 4]);
        assert_eq!(events_cursors(&request.requests)?, vec![Some(0), Some(2)]);

        let mut stuck = ScriptedConnection::new([Response::Events {
            events: lifecycle_events(4..=4),
            truncated: true,
        }]);
        assert_matches!(
            refresh_events(&mut stuck, &store, "svc").await,
            Err(ControlError::Unexpected(_))
        );
        Ok(())
    }

    /// An events change refreshes only the events of the service it names.
    #[tokio::test]
    async fn an_events_change_refreshes_the_mirrored_events() -> eyre::Result<()> {
        let store = store_with(entry_with_log(1, "line"));
        let mut request = ScriptedConnection::new([Response::Events {
            events: lifecycle_events(1..=1),
            truncated: false,
        }]);
        let (changes, mut received) = broadcast::channel(8);

        refresh_and_forward(
            &mut request,
            &store,
            &changes,
            SessionChange {
                service_id: "svc".to_string(),
                kind: ChangeKind::Events,
            },
        )
        .await?;

        assert_matches!(request.requests.as_slice(), [Request::GetEvents { .. }]);
        assert_eq!(mirrored_event_seqs(&store), vec![1]);
        assert_eq!(received.try_recv()?.kind, ChangeKind::Events);
        Ok(())
    }

    /// A session older than the events protocol is never asked for events, and the mirror says
    /// it has none to report rather than that there are none.
    #[tokio::test]
    async fn older_sessions_never_receive_events_requests() -> eyre::Result<()> {
        let store = store_with(entry_with_log(1, "line"));
        store.write().session.protocol_version = micromux_control::ProtocolVersion::new(3, 2);
        let mut request = ScriptedConnection::new([]);

        refresh_events(&mut request, &store, "svc").await?;

        assert!(request.requests.is_empty());
        assert_eq!(source_for(store).events("svc"), None);
        Ok(())
    }

    #[test]
    fn replacing_the_session_instance_clears_cached_state() {
        let store = store_with(entry_with_log(4, "old"));
        store.write().notice = Some("old rejection".to_string());

        let mut same_instance = session();
        same_instance.name = "renamed".to_string();
        update_session(&store, same_instance);
        assert!(store.read().services.contains_key("svc"));
        assert_eq!(store.read().notice.as_deref(), Some("old rejection"));

        let mut replacement = session();
        replacement.start_time = 2;
        update_session(&store, replacement);

        let guard = store.read();
        assert_eq!(guard.session.start_time, 2);
        assert!(guard.services.is_empty());
        assert_eq!(guard.notice, None);
    }

    #[test]
    fn acknowledged_restart_clears_logs_after_generation_advances() {
        let store = store_with(entry_with_log(4, "old"));
        mark_pending_log_clears(
            &store,
            &[ServiceCommandAck {
                service: "svc".to_string(),
                observed_generation: 1,
            }],
        );

        assert_eq!(
            reconcile_services(&store, vec![snapshot("svc", 1)]),
            [] as [std::string::String; 0]
        );
        assert_eq!(store.read().services["svc"].logs.len(), 1);

        assert_eq!(
            reconcile_services(&store, vec![snapshot("svc", 2)]),
            vec!["svc".to_string()]
        );
        let guard = store.read();
        let entry = &guard.services["svc"];
        assert!(entry.logs.is_empty());
        assert_eq!(entry.next_log_seq, 0);
        assert_eq!(entry.first_retained_seq, None);
        assert_eq!(entry.clear_logs_after_generation, None);
    }
}
