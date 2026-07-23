//! Client-side session mirror used by attach mode.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use indexmap::IndexMap;
use micromux::{
    CancellationToken, ChangeKind, HealthAttempt, LogLine, ServiceCommandAck, ServiceID,
    ServiceSnapshot, SessionChange,
};
use micromux_control::{
    Client, ControlEndpoint, ControlError, ErrorCode, Request, Response, SessionInfo, Subscription,
};
use parking_lot::RwLock;
use tokio::sync::{broadcast, mpsc};

use crate::source::AttachmentStatus;

const CHANGE_CHANNEL_CAPACITY: usize = 1024;
const COMMAND_CHANNEL_CAPACITY: usize = 64;
const REMOTE_LOG_TAIL: usize = 200;
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(250);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(5);

/// A synchronous mirror of a remote session, maintained by one background task.
pub struct RemoteSource {
    store: Arc<RwLock<MirrorStore>>,
    changes: broadcast::Sender<SessionChange>,
    commands: mpsc::Sender<RemoteCommand>,
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
    // Manual restarts clear visible logs without resetting their sequence. The acknowledged
    // generation distinguishes that clear from an automatic restart, which retains logs.
    clear_logs_after_generation: Option<u64>,
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

        let (commands, command_rx) = mpsc::channel(COMMAND_CHANNEL_CAPACITY);
        let shutdown = CancellationToken::new();
        tokio::spawn(run_remote(
            endpoint,
            Arc::clone(&store),
            changes.clone(),
            command_rx,
            shutdown.clone(),
            request,
            subscription,
        ));

        Ok(Self {
            store,
            changes,
            commands,
            shutdown,
        })
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
                let regressed = entry
                    .logs
                    .back()
                    .is_some_and(|newest| after >= newest.seq);
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

    /// Subscribe to mirror invalidations.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<SessionChange> {
        self.changes.subscribe()
    }

    /// Queue a remote service restart.
    pub fn restart(&self, id: ServiceID) {
        let _ = self.commands.try_send(RemoteCommand::Restart(id));
    }

    /// Queue a restart of every enabled service.
    pub fn restart_all(&self) {
        let _ = self.commands.try_send(RemoteCommand::RestartAll);
    }

    /// Queue a remote service enable.
    pub fn enable(&self, id: ServiceID) {
        let _ = self.commands.try_send(RemoteCommand::Enable(id));
    }

    /// Queue a remote service disable.
    pub fn disable(&self, id: ServiceID) {
        let _ = self.commands.try_send(RemoteCommand::Disable(id));
    }

    /// Queue retirement of a remote dynamic service.
    pub fn stop_dynamic(&self, id: ServiceID) {
        let _ = self.commands.try_send(RemoteCommand::StopDynamic(id));
    }

    /// Stop the mirror task without affecting the remote session.
    pub fn cancel(&self) {
        self.shutdown.cancel();
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
    mut commands: mpsc::Receiver<RemoteCommand>,
    shutdown: CancellationToken,
    mut request: Client,
    mut subscription: Subscription,
) {
    loop {
        match run_connected(
            &mut request,
            &mut subscription,
            &store,
            &changes,
            &mut commands,
            &shutdown,
        )
        .await
        {
            ConnectedLoopExit::Shutdown => return,
            ConnectedLoopExit::Reconnect => mark_disconnected(&store, &changes),
        }

        let mut delay = INITIAL_RECONNECT_DELAY;
        loop {
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
                    break;
                }
                Err(err) => {
                    tracing::debug!(?err, "remote mirror reconnect failed");
                    delay = delay.saturating_mul(2).min(MAX_RECONNECT_DELAY);
                }
            }
        }
    }
}

async fn run_connected(
    request: &mut Client,
    subscription: &mut Subscription,
    store: &Arc<RwLock<MirrorStore>>,
    changes: &broadcast::Sender<SessionChange>,
    commands: &mut mpsc::Receiver<RemoteCommand>,
    shutdown: &CancellationToken,
) -> ConnectedLoopExit {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return ConnectedLoopExit::Shutdown,
            command = commands.recv() => {
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
            received = subscription.recv() => {
                match received {
                    Ok(Some(change)) => {
                        if let Err(err) = refresh_and_forward(request, store, changes, change.clone()).await {
                            if reconnect_after_refresh_error(&err) {
                                return ConnectedLoopExit::Reconnect;
                            }
                            record_notice(store, changes, err.to_string());
                            publish(changes, change);
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
    let subscription = Client::subscribe(endpoint).await?;
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
    }
    Ok(store.read().services.keys().cloned().collect())
}

async fn refresh_and_forward<C: RequestConnection>(
    request: &mut C,
    store: &Arc<RwLock<MirrorStore>>,
    changes: &broadcast::Sender<SessionChange>,
    change: SessionChange,
) -> Result<(), ControlError> {
    match change.kind {
        ChangeKind::Logs => refresh_logs(request, store, &change.service_id).await?,
        ChangeKind::Health => refresh_health(request, store, &change.service_id).await?,
        ChangeKind::Status | ChangeKind::Roster | ChangeKind::Unknown => {
            refresh_roster(request, store).await?;
        }
        ChangeKind::Events => {}
    }
    publish(changes, change);
    Ok(())
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
    let Some((mut lines, mut truncated)) = request_log_page(request, service_id, after).await?
    else {
        store.write().services.shift_remove(service_id);
        return Ok(());
    };

    if cursor > 0
        && lines
            .first()
            .is_none_or(|line| line.seq > cursor.saturating_add(1))
    {
        let Some((tail, _)) = request_log_page(request, service_id, None).await? else {
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
        let Some((next_lines, next_truncated)) =
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
) -> Result<Option<(Vec<LogLine>, bool)>, ControlError> {
    match request
        .request(Request::FollowLogs {
            service: service_id.to_string(),
            run_generation: None,
            after,
        })
        .await?
    {
        Response::Logs { lines, truncated } => Ok(Some((lines, truncated))),
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
    matches!(error, ControlError::Io(_) | ControlError::Closed)
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
        for kind in [ChangeKind::Status, ChangeKind::Logs, ChangeKind::Health] {
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
    use super::*;
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
        async fn request(&mut self, request: Request) -> Result<Response, ControlError> {
            self.requests.push(request);
            self.responses.pop_front().ok_or_else(|| {
                ControlError::Unexpected("scripted connection exhausted".to_string())
            })
        }
    }

    fn session() -> SessionInfo {
        SessionInfo {
            protocol_version: micromux_control::PROTOCOL_VERSION,
            id: "session".to_string(),
            pid: 1,
            start_time: 1,
            name: "test".to_string(),
            working_dir: "/tmp".to_string(),
            config_path: "/tmp/micromux.yaml".to_string(),
            services: Vec::new(),
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
            clear_logs_after_generation: None,
        }
    }

    fn source_for(store: Arc<RwLock<MirrorStore>>) -> RemoteSource {
        let (changes, _) = broadcast::channel(8);
        let (commands, _) = mpsc::channel(8);
        RemoteSource {
            store,
            changes,
            commands,
            shutdown: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn discontinuity_clears_the_cache_and_retails() -> color_eyre::eyre::Result<()> {
        let store = store_with(entry_with_log(10, "old"));
        let mut request = ScriptedConnection::new([
            Response::Logs {
                lines: vec![line(15, "first retained"), line(16, "incremental")],
                truncated: false,
            },
            Response::Logs {
                lines: vec![line(20, "tail"), line(21, "latest")],
                truncated: false,
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
    async fn empty_incremental_page_clears_the_cache_and_retails() -> color_eyre::eyre::Result<()> {
        let store = store_with(entry_with_log(10, "old incarnation"));
        let mut request = ScriptedConnection::new([
            Response::Logs {
                lines: Vec::new(),
                truncated: false,
            },
            Response::Logs {
                lines: vec![line(11, "new line")],
                truncated: false,
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

    #[tokio::test]
    async fn same_sequence_snapshot_reaches_logs_since_through_the_mirror()
    -> color_eyre::eyre::Result<()> {
        let store = store_with(entry_with_log(7, "frame one"));
        let mut request = ScriptedConnection::new([Response::Logs {
            lines: vec![line(7, "frame two")],
            truncated: false,
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

    #[tokio::test]
    async fn refresh_stores_before_it_notifies() -> color_eyre::eyre::Result<()> {
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
    async fn session_wide_roster_change_reconciles_without_service_lookup()
    -> color_eyre::eyre::Result<()> {
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

        assert!(reconcile_services(&store, vec![snapshot("svc", 1)]).is_empty());
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
