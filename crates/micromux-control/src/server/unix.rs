use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use micromux::{
    ChangeKind, CommandRejection, SchedulerStopped, ServiceCommandResult, ServiceSnapshot,
    SessionChange, SessionModelReader, trim_to_last_bytes,
};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

#[path = "unix/log_reads.rs"]
mod log_reads;

use super::ControlServer;
use crate::endpoint::{CanonicalConfigPath, ControlEndpoint, project_lock_path};
use crate::protocol::{
    ErrorCode, PROTOCOL_VERSION, Request, Response, ServiceBrief, SessionInfo,
    supports_versioned_subscriptions,
};
use crate::{
    ControlError, Framing, IDLE_TIMEOUT, REQUEST_TIMEOUT, SUBSCRIPTION_HEARTBEAT_INTERVAL, framed,
    read_message, write_message,
};

/// Default number of recent log lines returned when a client does not specify `tail`.
const DEFAULT_LOG_TAIL: usize = 200;
/// Hard cap on `tail`, independent of the request frame.
const MAX_LOG_TAIL: usize = 2000;
/// Default number of recent timeline events returned when a client does not specify `tail`.
const DEFAULT_EVENT_TAIL: usize = 50;
/// Hard cap on the event `tail`; the model retains at most this many events per service.
const MAX_EVENT_TAIL: usize = micromux::EVENT_HISTORY;
/// Change kinds replayed per surviving service after broadcast loss. `Roster` is deliberately
/// absent: the roster invalidation is session-wide and sent once, ahead of these.
const LAG_REPLAY_SERVICE_KINDS: [ChangeKind; 4] = [
    ChangeKind::Status,
    ChangeKind::Logs,
    ChangeKind::Health,
    ChangeKind::Events,
];
/// Response byte budget below the framing limit, leaving room for protocol overhead.
const RESPONSE_MAX_BYTES: usize = 512 * 1024;
/// Maximum number of clients served concurrently by one session.
const MAX_CONNECTIONS: usize = 64;
/// Complete frames a subscription peer may send before the server treats it as abusive.
const MAX_UNSOLICITED_SUBSCRIPTION_FRAMES: usize = 32;
/// A stalled subscriber must release its connection permit promptly.
const SUBSCRIPTION_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
/// A bound control endpoint plus the lifetime ownership lock. Dropping it unlinks the socket while
/// the lock is still held, so a successor (which cannot acquire the lock until this process exits)
/// never has its fresh socket removed.
pub struct EndpointGuard {
    listener: tokio::net::UnixListener,
    socket_path: PathBuf,
    // Held for the whole process lifetime; the OS releases the advisory lock on exit (incl. crash).
    _lock: std::fs::File,
    // Kept under fixed per-user `/tmp`, independent of the endpoint's XDG/TMP runtime directory.
    _project_lock: Option<std::fs::File>,
}

impl Drop for EndpointGuard {
    fn drop(&mut self) {
        // Unlink the socket under the still-held lock. The permanent `<hash>.lock` file is left in
        // place; only its advisory lock is released (when `_lock` drops).
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

pub(super) fn bind(endpoint: &ControlEndpoint) -> Result<Option<EndpointGuard>, ControlError> {
    match endpoint {
        ControlEndpoint::Unix(path) => bind_unix(path, None),
        ControlEndpoint::WindowsNamedPipe(_) => Err(ControlError::Unsupported),
    }
}

pub(super) fn bind_project(
    endpoint: &ControlEndpoint,
    config_path: &CanonicalConfigPath,
) -> Result<Option<EndpointGuard>, ControlError> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let lock_path = project_lock_path(config_path)?;
    let project_lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)?;
    project_lock.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    match fs2::FileExt::try_lock_exclusive(&project_lock) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    match endpoint {
        ControlEndpoint::Unix(path) => bind_unix(path, Some(project_lock)),
        ControlEndpoint::WindowsNamedPipe(_) => Err(ControlError::Unsupported),
    }
}

pub(super) fn endpoint_owner_lock_held(endpoint: &ControlEndpoint) -> Result<bool, ControlError> {
    match endpoint {
        ControlEndpoint::Unix(path) => owner_lock_held_unix(path),
        ControlEndpoint::WindowsNamedPipe(_) => Err(ControlError::Unsupported),
    }
}

fn bind_unix(
    socket_path: &Path,
    project_lock: Option<std::fs::File>,
) -> Result<Option<EndpointGuard>, ControlError> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    let lock_path = socket_path.with_extension("lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)?;
    lock_file.set_permissions(std::fs::Permissions::from_mode(0o600))?;

    // The lifetime-held lock is the authoritative ownership signal — more robust than connect-probing
    // a possibly-wedged listener. "lock acquirable" ⇔ "no live owner".
    match fs2::FileExt::try_lock_exclusive(&lock_file) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
        Err(err) => return Err(err.into()),
    }

    // We hold the lock ⇒ no live owner: unlink any crash-leaked socket and bind.
    if let Err(err) = std::fs::remove_file(socket_path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        return Err(err.into());
    }
    let listener = tokio::net::UnixListener::bind(socket_path)?;
    // Defence in depth: the directory mode (0700) is what actually gates `connect`.
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;

    Ok(Some(EndpointGuard {
        listener,
        socket_path: socket_path.to_path_buf(),
        _lock: lock_file,
        _project_lock: project_lock,
    }))
}

fn owner_lock_held_unix(socket_path: &Path) -> Result<bool, ControlError> {
    let lock_path = socket_path.with_extension("lock");
    let lock_file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
    {
        Ok(lock_file) => lock_file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };

    match fs2::FileExt::try_lock_exclusive(&lock_file) {
        Ok(()) => Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
        Err(err) => Err(err.into()),
    }
}

pub(super) async fn serve(
    server: Arc<ControlServer>,
    guard: EndpointGuard,
    shutdown: CancellationToken,
) -> Result<(), ControlError> {
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = guard.listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let Ok(permit) = Arc::clone(&connections).try_acquire_owned() else {
                            tracing::debug!("control: connection limit reached");
                            continue;
                        };
                        let server = Arc::clone(&server);
                        let shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            handle_connection(server, stream, shutdown).await;
                        });
                    }
                    Err(err) => {
                        // Back off briefly so a persistent accept error (e.g. EMFILE) cannot hot-spin.
                        tracing::warn!(?err, "control: accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
            }
        }
    }
    // Dropping `guard` here unlinks the socket while the lock is still held.
    drop(guard);
    Ok(())
}

async fn handle_connection<S>(server: Arc<ControlServer>, stream: S, shutdown: CancellationToken)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut conn = framed(stream);
    loop {
        let read = tokio::select! {
            () = shutdown.cancelled() => return,
            read = tokio::time::timeout(IDLE_TIMEOUT, read_message::<S, Request>(&mut conn)) => read,
        };
        let request = match read {
            Ok(Ok(Some(request))) => request,
            Ok(Err(err)) => {
                tracing::debug!(?err, "control: rejecting bad request frame");
                let response = Response::error(ErrorCode::BadRequest, err.to_string());
                let _ = write_response(&mut conn, &response, &shutdown).await;
                return;
            }
            // Client disconnected (None) or idle timeout (Err): close the connection.
            Ok(Ok(None)) | Err(_) => return,
        };

        match request {
            Request::Subscribe => {
                stream_changes(&server, conn, shutdown, None).await;
                return;
            }
            Request::SubscribeWithVersion { protocol_version } => {
                let heartbeat = supports_versioned_subscriptions(protocol_version)
                    .then_some(SUBSCRIPTION_HEARTBEAT_INTERVAL);
                stream_changes(&server, conn, shutdown, heartbeat).await;
                return;
            }
            _ => {}
        }

        if matches!(request, Request::Shutdown) {
            // Acknowledge first, then cancel the shared session token: the accept loop, scheduler,
            // and TUI all observe it, so this stops the whole session (the same path as Ctrl-C).
            // Writing before cancelling ensures the client sees the ack before the endpoint vanishes.
            let _ = write_response(&mut conn, &Response::ShuttingDown, &shutdown).await;
            shutdown.cancel();
            return;
        }

        // Read-only requests return instantly; a mutation awaits the scheduler, so bound it — a
        // wedged scheduler must not pin this task, and shutdown must stay responsive during dispatch.
        let response = tokio::select! {
            () = shutdown.cancelled() => return,
            dispatched = tokio::time::timeout(REQUEST_TIMEOUT, dispatch(&server, request)) => {
                match dispatched {
                    Ok(response) => response,
                    Err(_) => {
                        Response::error(ErrorCode::Timeout, "the scheduler did not respond in time")
                    }
                }
            }
        };
        if !write_response(&mut conn, &response, &shutdown).await {
            return;
        }
    }
}

async fn write_response<S>(
    conn: &mut Framing<S>,
    response: &Response,
    shutdown: &CancellationToken,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let response = response_within_frame(response);
    tokio::select! {
        () = shutdown.cancelled() => false,
        result = tokio::time::timeout(REQUEST_TIMEOUT, write_message(conn, response.as_ref())) => {
            matches!(result, Ok(Ok(())))
        }
    }
}

fn response_within_frame(response: &Response) -> Cow<'_, Response> {
    if encoded_len(response).is_some_and(|len| len <= RESPONSE_MAX_BYTES) {
        return Cow::Borrowed(response);
    }
    if let Response::Description(info) = response
        && let Some(info) = bounded_description(info)
    {
        return Cow::Owned(Response::Description(info));
    }

    Cow::Owned(Response::error(
        ErrorCode::LimitExceeded,
        "the response exceeds the control protocol frame budget",
    ))
}

fn bounded_description(info: &SessionInfo) -> Option<SessionInfo> {
    let mut info = info.clone();
    let services = std::mem::take(&mut info.services);
    info.services_truncated = true;
    let mut low = 0;
    let mut high = services.len();
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        info.services = services.get(..middle)?.to_vec();
        if encoded_len(&Response::Description(info.clone()))
            .is_some_and(|len| len <= RESPONSE_MAX_BYTES)
        {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    info.services = services.get(..low)?.to_vec();
    encoded_len(&Response::Description(info.clone()))
        .is_some_and(|len| len <= RESPONSE_MAX_BYTES)
        .then_some(info)
}

async fn stream_changes<S>(
    server: &ControlServer,
    conn: Framing<S>,
    shutdown: CancellationToken,
    heartbeat_interval: Option<std::time::Duration>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    use tokio::sync::broadcast::error::RecvError;

    let (mut sink, mut stream) = conn.split();
    let mut changes = server.reader.subscribe();
    let mut heartbeat = heartbeat_interval.map(|interval| Box::pin(tokio::time::sleep(interval)));
    let mut unsolicited_frames = 0usize;
    'stream: loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = async {
                match heartbeat.as_mut() {
                    Some(timer) => timer.as_mut().await,
                    None => std::future::pending().await,
                }
            } => {
                let change = SessionChange {
                    service_id: SessionChange::SESSION_WIDE.to_string(),
                    kind: ChangeKind::Heartbeat,
                };
                if !send_stream_change(&mut sink, change, &shutdown).await {
                    break;
                }
                if let (Some(timer), Some(interval)) =
                    (heartbeat.as_mut(), heartbeat_interval)
                {
                    timer.as_mut().reset(tokio::time::Instant::now() + interval);
                }
            }
            incoming = stream.next() => {
                match incoming {
                    None | Some(Err(_)) => break,
                    // Subscription clients have no request/response exchange after subscribing.
                    // Ignore complete frames so a forward-compatible keepalive cannot tear down
                    // an otherwise healthy stream.
                    Some(Ok(_)) => {
                        unsolicited_frames = unsolicited_frames.saturating_add(1);
                        if unsolicited_frames > MAX_UNSOLICITED_SUBSCRIPTION_FRAMES {
                            break;
                        }
                    }
                }
            }
            change = changes.recv() => {
                match change {
                    Ok(change) => {
                        if !send_stream_change(&mut sink, change, &shutdown).await {
                            break;
                        }
                        if let (Some(timer), Some(interval)) =
                            (heartbeat.as_mut(), heartbeat_interval)
                        {
                            timer.as_mut().reset(tokio::time::Instant::now() + interval);
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        let snapshots = server.reader.services();
                        for change in lag_replay_changes(&snapshots) {
                            if !send_stream_change(&mut sink, change, &shutdown).await {
                                break 'stream;
                            }
                        }
                        if let (Some(timer), Some(interval)) =
                            (heartbeat.as_mut(), heartbeat_interval)
                        {
                            timer.as_mut().reset(tokio::time::Instant::now() + interval);
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }
}

async fn send_stream_change<S>(
    sink: &mut futures::stream::SplitSink<Framing<S>, String>,
    change: SessionChange,
    shutdown: &CancellationToken,
) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Ok(line) = serde_json::to_string(&Response::Change(change)) else {
        return false;
    };
    tokio::select! {
        () = shutdown.cancelled() => false,
        result = tokio::time::timeout(SUBSCRIPTION_WRITE_TIMEOUT, sink.send(line)) => {
            matches!(result, Ok(Ok(())))
        }
    }
}

/// Synthesizes a complete invalidation after broadcast loss.
///
/// The roster notification is unconditional because the current roster may be empty after the
/// client lagged across removal of its final service. Its service id is
/// [`SessionChange::SESSION_WIDE`]; consumers re-read the whole roster for this change kind.
fn lag_replay_changes(snapshots: &[ServiceSnapshot]) -> impl Iterator<Item = SessionChange> + '_ {
    std::iter::once(SessionChange {
        service_id: SessionChange::SESSION_WIDE.to_string(),
        kind: ChangeKind::Roster,
    })
    .chain(snapshots.iter().flat_map(|snapshot| {
        LAG_REPLAY_SERVICE_KINDS.map(|kind| SessionChange {
            service_id: snapshot.id.clone(),
            kind,
        })
    }))
}

async fn dispatch(server: &ControlServer, request: Request) -> Response {
    match request {
        Request::Describe => Response::Description(describe(server)),
        Request::ListServices => Response::Services(server.reader.services()),
        Request::GetService { service } => {
            let snapshots = server.reader.services();
            snapshots
                .iter()
                .find(|snapshot| snapshot.id == service)
                .or_else(|| snapshots.iter().find(|snapshot| snapshot.name == service))
                .cloned()
                .map_or_else(
                    || unknown_service(&service),
                    |snapshot| Response::Service(Box::new(snapshot)),
                )
        }
        Request::GetLogs {
            service,
            run_generation,
            tail,
        } => {
            if run_generation.is_none() {
                get_logs(&server.reader, &service, None, tail)
            } else {
                let reader = server.reader.clone();
                log_reads::run(move || get_logs(&reader, &service, run_generation, tail)).await
            }
        }
        Request::FollowLogs {
            service,
            run_generation,
            after,
        } => {
            if run_generation.is_none() {
                follow_logs(&server.reader, &service, None, after)
            } else {
                let reader = server.reader.clone();
                log_reads::run(move || follow_logs(&reader, &service, run_generation, after)).await
            }
        }
        Request::ListLogRuns { service } => {
            let reader = server.reader.clone();
            log_reads::run(move || list_log_runs(&reader, &service)).await
        }
        Request::GetHealth { service } => {
            if server.reader.service(&service).is_none() {
                return unknown_service(&service);
            }
            let mut attempt = server.reader.latest_health(&service);
            bound_health_attempt(&mut attempt);
            Response::Health(attempt)
        }
        Request::GetHealthHistory { service } => get_health_history(&server.reader, &service),
        Request::GetEvents {
            service,
            after,
            tail,
        } => get_events(&server.reader, &service, after, tail),
        Request::Restart { service } => acknowledge(server.control.restart(&service).await),
        Request::RestartAll => acknowledge(server.control.restart_all().await),
        Request::Enable { service } => acknowledge(server.control.enable(&service).await),
        Request::Disable { service } => acknowledge(server.control.disable(&service).await),
        Request::StartDynamicService { params } => {
            acknowledge_dynamic(server.control.start_dynamic(params).await)
        }
        Request::ReplaceDynamicService {
            service,
            expected_revision,
            params,
        } => acknowledge_dynamic(
            server
                .control
                .replace_dynamic(&service, expected_revision, params)
                .await,
        ),
        Request::RenewDynamicService {
            service,
            expected_revision,
            expires_after,
        } => acknowledge_dynamic(
            server
                .control
                .renew_dynamic(&service, expected_revision, expires_after)
                .await,
        ),
        Request::StopDynamicService { service } => {
            acknowledge_dynamic(server.control.stop_dynamic(&service).await)
        }
        Request::ReconcileConfig { dry_run } => reconcile(server, dry_run).await,
        // Subscribe is intercepted before dispatch; reaching here is a protocol misuse.
        Request::Subscribe | Request::SubscribeWithVersion { .. } => Response::error(
            ErrorCode::BadRequest,
            "subscribe must be the only request on a connection",
        ),
        // Shutdown is intercepted before dispatch; reaching here is a protocol misuse.
        Request::Shutdown => {
            Response::error(ErrorCode::BadRequest, "shutdown is handled before dispatch")
        }
    }
}

async fn reconcile(server: &ControlServer, dry_run: bool) -> Response {
    acknowledge_reconcile(server.control.reconcile_config(dry_run).await)
}

fn get_events(
    reader: &SessionModelReader,
    service: &str,
    after: Option<u64>,
    tail: Option<usize>,
) -> Response {
    if reader.service(service).is_none() {
        return unknown_service(service);
    }
    let requested_tail = tail.unwrap_or(DEFAULT_EVENT_TAIL);
    let (events, truncated) =
        reader.events(service, after, Some(requested_tail.min(MAX_EVENT_TAIL)));
    Response::Events { events, truncated }
}

fn list_log_runs(reader: &SessionModelReader, service: &str) -> Response {
    if reader.service(service).is_none() {
        return unknown_service(service);
    }
    Response::LogRuns {
        runs: reader.log_runs(service),
    }
}

fn get_logs(
    reader: &SessionModelReader,
    service: &str,
    run_generation: Option<u64>,
    tail: Option<usize>,
) -> Response {
    if reader.service(service).is_none() {
        return unknown_service(service);
    }
    let requested_tail = tail.unwrap_or(if run_generation.is_some() {
        MAX_LOG_TAIL
    } else {
        DEFAULT_LOG_TAIL
    });
    let tail = requested_tail.min(MAX_LOG_TAIL);
    let mut truncated = false;
    let mut lines = match run_generation {
        Some(run_generation) => {
            let run = match reader.try_run_log(service, run_generation, Some(tail)) {
                Ok(Some(run)) => run,
                Ok(None) => return unknown_run(service, run_generation),
                Err(err) => {
                    return Response::error(ErrorCode::LimitExceeded, err.to_string());
                }
            };
            run.lines
        }
        None => reader.logs(service, Some(tail)),
    };
    truncated |= requested_tail > MAX_LOG_TAIL && lines.len() >= tail;
    if let Some(run_generation) = run_generation
        && tail == MAX_LOG_TAIL
    {
        truncated |= reader
            .log_runs(service)
            .into_iter()
            .find(|run| run.run_generation == run_generation)
            .is_some_and(|run| run.line_count > lines.len());
    }
    let first_retained_seq = run_generation
        .is_none()
        .then(|| reader.first_retained_log_seq(service))
        .flatten();
    truncated |= bound_tail_response_lines(&mut lines, first_retained_seq);
    logs_response(lines, truncated, first_retained_seq)
}

fn follow_logs(
    reader: &SessionModelReader,
    service: &str,
    run_generation: Option<u64>,
    after: Option<u64>,
) -> Response {
    if reader.service(service).is_none() {
        return unknown_service(service);
    }

    // A retained disk run pages strictly forward from `after` — or from the very beginning when it
    // is omitted — keeping the oldest contiguous page, so a run longer than MAX_LOG_TAIL is fully
    // reachable by following `next_seq` rather than being pinned to its tail.
    if let Some(run_generation) = run_generation {
        let run = match reader.try_run_log_after(
            service,
            run_generation,
            after,
            Some(MAX_LOG_TAIL + 1),
        ) {
            Ok(Some(run)) => run,
            Ok(None) => return unknown_run(service, run_generation),
            Err(err) => {
                return Response::error(ErrorCode::LimitExceeded, err.to_string());
            }
        };
        let mut lines = run.lines;
        if let Some(cursor) = after {
            lines.retain(|line| line.seq > cursor);
        }
        let (lines, truncated) = bound_follow_response_lines_page(lines, None);
        return logs_response(lines, truncated, None);
    }

    // The bounded visible stream: page forward from a cursor, else return its most recent tail.
    if let Some(cursor) = after {
        let (first_retained_seq, lines) = reader.logs_since(service, cursor);
        let (lines, truncated) = bound_follow_response_lines_page(lines, first_retained_seq);
        return logs_response(lines, truncated, first_retained_seq);
    }

    let mut lines = reader.logs(service, None);
    let first_retained_seq = lines.first().map(|line| line.seq);
    let mut truncated = false;
    if lines.len() > MAX_LOG_TAIL {
        let drop = lines.len() - MAX_LOG_TAIL;
        lines.drain(0..drop);
        truncated = true;
    }
    truncated |= bound_tail_response_lines(&mut lines, first_retained_seq);
    logs_response(lines, truncated, first_retained_seq)
}

fn bound_follow_response_lines_page(
    mut lines: Vec<micromux::LogLine>,
    first_retained_seq: Option<u64>,
) -> (Vec<micromux::LogLine>, bool) {
    let capped = lines.len() > MAX_LOG_TAIL;
    lines.truncate(MAX_LOG_TAIL);
    let truncated = capped || bound_follow_response_lines(&mut lines, first_retained_seq);
    (lines, truncated)
}

fn describe(server: &ControlServer) -> SessionInfo {
    let snapshots = server.reader.services();
    let services = snapshots
        .iter()
        .map(|snapshot| ServiceBrief {
            id: snapshot.id.clone(),
            name: snapshot.name.clone(),
        })
        .collect();
    let dynamic_services = server.dynamic_policy.enabled.then(|| {
        let live_services = snapshots
            .iter()
            .filter(|snapshot| {
                snapshot.origin == micromux::OriginKind::Dynamic && snapshot.retired.is_none()
            })
            .count();
        crate::DynamicServicesCaps {
            max_services: server.dynamic_policy.max_services,
            live_services,
            max_lifetime_secs: server
                .dynamic_policy
                .max_lifetime
                .map(|value| value.as_secs()),
            allowed_working_roots: server
                .dynamic_policy
                .allowed_working_roots
                .iter()
                .map(|root| root.display().to_string())
                .collect(),
        }
    });
    SessionInfo {
        protocol_version: PROTOCOL_VERSION,
        id: server.identity.id.clone(),
        pid: server.identity.pid,
        start_time: server.identity.start_time,
        name: server.identity.name.clone(),
        working_dir: server.identity.working_dir.clone(),
        config_path: server.identity.config_path.clone(),
        services,
        services_truncated: false,
        micromux_version: server.identity.micromux_version.clone(),
        capabilities: Some(crate::SessionCapabilities { dynamic_services }),
    }
}

fn unknown_service(service: &str) -> Response {
    Response::error(
        ErrorCode::UnknownService,
        format!("unknown service `{service}`"),
    )
}

fn unknown_run(service: &str, run_generation: u64) -> Response {
    Response::error(
        ErrorCode::UnknownRun,
        format!("service `{service}` has no retained run `{run_generation}`"),
    )
}

fn logs_response(
    lines: Vec<micromux::LogLine>,
    truncated: bool,
    first_retained_seq: Option<u64>,
) -> Response {
    Response::Logs {
        lines,
        truncated,
        first_retained_seq,
    }
}

/// Drop the oldest records until the encoded response, including JSON escaping, fits.
fn bound_tail_response_lines(
    lines: &mut Vec<micromux::LogLine>,
    first_retained_seq: Option<u64>,
) -> bool {
    if logs_fit(lines, true, first_retained_seq) {
        return false;
    }

    let mut low = 0;
    let mut high = lines.len();
    while low < high {
        let middle = low + (high - low) / 2;
        if lines
            .get(middle..)
            .is_some_and(|lines| logs_fit(lines, true, first_retained_seq))
        {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    if low == lines.len() {
        let retained = lines
            .last()
            .cloned()
            .and_then(|line| fit_single_log_line(line, true, first_retained_seq));
        lines.clear();
        lines.extend(retained);
    } else {
        lines.drain(..low);
    }
    true
}

/// Keep the oldest contiguous page whose encoded response fits.
fn bound_follow_response_lines(
    lines: &mut Vec<micromux::LogLine>,
    first_retained_seq: Option<u64>,
) -> bool {
    if logs_fit(lines, true, first_retained_seq) {
        return false;
    }

    let mut low = 0;
    let mut high = lines.len();
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if lines
            .get(..middle)
            .is_some_and(|lines| logs_fit(lines, true, first_retained_seq))
        {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    if low == 0 {
        let retained = lines
            .first()
            .cloned()
            .and_then(|line| fit_single_log_line(line, false, first_retained_seq));
        lines.clear();
        lines.extend(retained);
    } else {
        lines.truncate(low);
    }
    true
}

fn fit_single_log_line(
    mut line: micromux::LogLine,
    keep_tail: bool,
    first_retained_seq: Option<u64>,
) -> Option<micromux::LogLine> {
    let original = std::mem::take(&mut line.line);
    let mut boundaries = if keep_tail {
        original
            .char_indices()
            .map(|(index, _)| original.len().saturating_sub(index))
            .collect::<Vec<_>>()
    } else {
        original
            .char_indices()
            .map(|(index, _)| index)
            .collect::<Vec<_>>()
    };
    boundaries.push(0);
    boundaries.push(original.len());
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut low = 0usize;
    let mut high = boundaries.len().saturating_sub(1);
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        let bytes = boundaries.get(middle).copied().unwrap_or_default();
        line.line = if keep_tail {
            original
                .get(original.len().saturating_sub(bytes)..)
                .unwrap_or_default()
                .to_string()
        } else {
            original.get(..bytes).unwrap_or_default().to_string()
        };
        if logs_fit(std::slice::from_ref(&line), true, first_retained_seq) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let bytes = boundaries.get(low).copied().unwrap_or_default();
    line.line = if keep_tail {
        original
            .get(original.len().saturating_sub(bytes)..)
            .unwrap_or_default()
            .to_string()
    } else {
        original.get(..bytes).unwrap_or_default().to_string()
    };
    logs_fit(std::slice::from_ref(&line), true, first_retained_seq).then_some(line)
}

fn logs_fit(lines: &[micromux::LogLine], truncated: bool, first_retained_seq: Option<u64>) -> bool {
    encoded_len(&Response::Logs {
        lines: lines.to_vec(),
        truncated,
        first_retained_seq,
    })
    .is_some_and(|len| len <= RESPONSE_MAX_BYTES)
}

fn encoded_len(value: &impl Serialize) -> Option<usize> {
    serde_json::to_vec(value).ok().map(|encoded| encoded.len())
}

fn bound_health_attempt(attempt: &mut Option<micromux::HealthAttempt>) {
    let Some(attempt) = attempt else {
        return;
    };

    bound_health_attempt_to(attempt, RESPONSE_MAX_BYTES);
}

fn get_health_history(reader: &SessionModelReader, service: &str) -> Response {
    if reader.service(service).is_none() {
        return unknown_service(service);
    }
    let mut attempts = reader.healthchecks(service);
    bound_health_history(&mut attempts);
    Response::HealthHistory { attempts }
}

fn bound_health_attempt_to(attempt: &mut micromux::HealthAttempt, max_bytes: usize) {
    let mut total = attempt.command.len()
        + attempt
            .output
            .iter()
            .map(|line| line.line.len())
            .sum::<usize>();
    let mut drop_count = 0;
    for line in &attempt.output {
        if total <= max_bytes {
            break;
        }
        total = total.saturating_sub(line.line.len());
        drop_count += 1;
    }
    if drop_count > 0 {
        attempt.output.drain(0..drop_count);
    }
    if total > max_bytes {
        attempt.command = trim_to_last_bytes(std::mem::take(&mut attempt.command), max_bytes);
    }
}

fn health_attempt_bytes(attempt: &micromux::HealthAttempt) -> usize {
    attempt.command.len()
        + attempt
            .output
            .iter()
            .map(|line| line.line.len())
            .sum::<usize>()
}

/// Drop whole oldest attempts before trimming content from the oldest attempt that survives.
fn bound_health_history(attempts: &mut Vec<micromux::HealthAttempt>) {
    let mut total = attempts.iter().map(health_attempt_bytes).sum::<usize>();
    let mut drop_count = 0;
    for attempt in attempts.iter() {
        if total <= RESPONSE_MAX_BYTES || attempts.len().saturating_sub(drop_count) <= 1 {
            break;
        }
        total = total.saturating_sub(health_attempt_bytes(attempt));
        drop_count += 1;
    }
    if drop_count > 0 {
        attempts.drain(0..drop_count);
    }
    if let Some(attempt) = attempts.first_mut()
        && total > RESPONSE_MAX_BYTES
    {
        bound_health_attempt_to(attempt, RESPONSE_MAX_BYTES);
    }
}

/// Map a scheduler rejection to its wire error. Shared by every mutation acknowledgement so a
/// new [`CommandRejection`] variant only needs handling once.
fn rejection_response(rejection: CommandRejection) -> Response {
    match rejection {
        CommandRejection::UnknownService => {
            Response::error(ErrorCode::UnknownService, "unknown service")
        }
        CommandRejection::InvalidState(message) => {
            Response::error(ErrorCode::InvalidState, message)
        }
        CommandRejection::ConfigReload(message) => {
            Response::error(ErrorCode::ConfigReload, message)
        }
        CommandRejection::PolicyDenied(message) => {
            Response::error(ErrorCode::PolicyDenied, message)
        }
        CommandRejection::LimitExceeded(message) => {
            Response::error(ErrorCode::LimitExceeded, message)
        }
        CommandRejection::RevisionMismatch { expected, actual } => Response::error(
            ErrorCode::RevisionMismatch,
            format!("expected revision {expected}, actual revision {actual}"),
        ),
        CommandRejection::InvalidSpec(message) => Response::error(ErrorCode::InvalidSpec, message),
    }
}

/// Shared acknowledgement plumbing: only the success payload differs between the mutation
/// families, so rejections and a stopped scheduler are mapped exactly once.
fn acknowledge_with<T>(
    result: Result<Result<T, CommandRejection>, SchedulerStopped>,
    ok: impl FnOnce(T) -> Response,
) -> Response {
    match result {
        Ok(Ok(value)) => ok(value),
        Ok(Err(rejection)) => rejection_response(rejection),
        Err(SchedulerStopped) => {
            Response::error(ErrorCode::SchedulerStopped, "the scheduler has stopped")
        }
    }
}

fn acknowledge(result: Result<ServiceCommandResult, SchedulerStopped>) -> Response {
    acknowledge_with(result, |services| Response::Accepted { services })
}

fn acknowledge_dynamic(
    result: Result<micromux::DynamicServiceResult, SchedulerStopped>,
) -> Response {
    acknowledge_with(result, Response::DynamicService)
}

fn acknowledge_reconcile(result: Result<micromux::ReconcileResult, SchedulerStopped>) -> Response {
    acknowledge_with(result, Response::Reconcile)
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use color_eyre::eyre;
    use micromux::{
        ChangeKind, HealthAttempt, HealthLine, LogLine, OutputStream, RestartPolicy,
        ServiceSnapshot, SessionChange,
    };
    use similar_asserts::assert_eq;

    use crate::{
        CanonicalConfigPath, ControlEndpoint, ControlServer, ErrorCode, PROTOCOL_VERSION, Request,
        Response, ServiceBrief, SessionIdentity, SessionInfo,
    };

    use super::{
        MAX_LOG_TAIL, MAX_UNSOLICITED_SUBSCRIPTION_FRAMES, RESPONSE_MAX_BYTES, bind_project,
        bound_follow_response_lines, bound_follow_response_lines_page, bound_health_history,
        bound_tail_response_lines, encoded_len, health_attempt_bytes, lag_replay_changes, logs_fit,
        project_lock_path, response_within_frame, stream_changes,
    };

    fn line(seq: u64, len: usize) -> LogLine {
        LogLine {
            seq,
            run_generation: 1,
            timestamp_unix_ms: 1_700_000_000_000 + seq,
            line: "x".repeat(len),
        }
    }

    fn snapshot(id: &str) -> ServiceSnapshot {
        ServiceSnapshot::initial(
            id.to_string(),
            id.to_string(),
            Vec::new(),
            None,
            RestartPolicy::Never,
            vec!["true".to_string()],
            None,
        )
    }

    fn health_attempt(attempt: u64, output_bytes: usize) -> HealthAttempt {
        HealthAttempt {
            run_generation: 1,
            attempt,
            command: "probe".to_string(),
            output: vec![HealthLine {
                stream: OutputStream::Stdout,
                line: "x".repeat(output_bytes),
            }],
            result: None,
        }
    }

    #[test]
    fn health_history_bounding_drops_oldest_attempts_before_trimming() {
        let mut attempts = vec![
            health_attempt(1, RESPONSE_MAX_BYTES / 2),
            health_attempt(2, RESPONSE_MAX_BYTES / 2),
        ];

        bound_health_history(&mut attempts);

        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts.first().map(|attempt| attempt.attempt), Some(2));
        assert!(attempts.iter().map(health_attempt_bytes).sum::<usize>() <= RESPONSE_MAX_BYTES);
    }

    #[test]
    fn health_history_bounding_trims_an_oversized_surviving_attempt() {
        let mut attempts = vec![health_attempt(7, RESPONSE_MAX_BYTES + 100)];

        bound_health_history(&mut attempts);

        assert_eq!(attempts.first().map(|attempt| attempt.attempt), Some(7));
        assert!(attempts.iter().map(health_attempt_bytes).sum::<usize>() <= RESPONSE_MAX_BYTES);
    }

    #[test]
    fn lag_replay_invalidates_an_empty_roster() {
        let changes = lag_replay_changes(&[]).collect::<Vec<_>>();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].service_id, SessionChange::SESSION_WIDE);
        assert_eq!(changes[0].kind, ChangeKind::Roster);
    }

    #[test]
    fn lag_replay_invalidates_the_roster_once_and_each_surviving_service() {
        let snapshots = [snapshot("alpha"), snapshot("beta")];
        let changes = lag_replay_changes(&snapshots)
            .map(|change| (change.service_id, change.kind))
            .collect::<Vec<_>>();

        assert_eq!(
            changes,
            vec![
                (SessionChange::SESSION_WIDE.to_string(), ChangeKind::Roster),
                ("alpha".to_string(), ChangeKind::Status),
                ("alpha".to_string(), ChangeKind::Logs),
                ("alpha".to_string(), ChangeKind::Health),
                ("alpha".to_string(), ChangeKind::Events),
                ("beta".to_string(), ChangeKind::Status),
                ("beta".to_string(), ChangeKind::Logs),
                ("beta".to_string(), ChangeKind::Health),
                ("beta".to_string(), ChangeKind::Events),
            ]
        );
    }

    #[test]
    fn oversized_unpageable_response_becomes_a_readable_limit_error() {
        let response = Response::Services(vec![snapshot(&"\\\"".repeat(RESPONSE_MAX_BYTES))]);

        let bounded = response_within_frame(&response);

        assert!(matches!(
            bounded.as_ref(),
            Response::Error {
                code: ErrorCode::LimitExceeded,
                ..
            }
        ));
        assert!(encoded_len(bounded.as_ref()).is_some_and(|len| len <= crate::MAX_FRAME_BYTES));
    }

    #[test]
    fn oversized_description_keeps_identity_and_marks_its_service_index() {
        let services = (0..8)
            .map(|index| ServiceBrief {
                id: format!("service-{index}"),
                name: "x".repeat(100 * 1024),
            })
            .collect::<Vec<_>>();
        let info = SessionInfo {
            protocol_version: PROTOCOL_VERSION,
            id: "session-id".to_string(),
            pid: 42,
            start_time: 7,
            name: "session".to_string(),
            working_dir: "/project".to_string(),
            config_path: "/project/micromux.yaml".to_string(),
            services,
            services_truncated: false,
            micromux_version: "test".to_string(),
            capabilities: None,
        };

        let response = Response::Description(info.clone());
        let bounded = response_within_frame(&response);
        let Response::Description(bounded_info) = bounded.as_ref() else {
            panic!("description should remain readable");
        };

        assert_eq!(bounded_info.id, info.id);
        assert!(bounded_info.services_truncated);
        assert!(bounded_info.services.len() < info.services.len());
        assert!(encoded_len(bounded.as_ref()).is_some_and(|len| len <= RESPONSE_MAX_BYTES));
    }

    #[test]
    fn tail_bounding_keeps_the_newest_lines() {
        let mut lines = vec![
            line(0, 200 * 1024),
            line(1, 200 * 1024),
            line(2, 200 * 1024),
            line(3, 200 * 1024),
        ];

        assert!(bound_tail_response_lines(&mut lines, None));

        let seqs: Vec<u64> = lines.into_iter().map(|line| line.seq).collect();
        assert_eq!(seqs, vec![2, 3]);
    }

    #[test]
    fn follow_bounding_keeps_the_oldest_contiguous_page() {
        let mut lines = vec![
            line(0, 200 * 1024),
            line(1, 200 * 1024),
            line(2, 200 * 1024),
            line(3, 200 * 1024),
        ];

        assert!(bound_follow_response_lines(&mut lines, None));

        let seqs: Vec<u64> = lines.into_iter().map(|line| line.seq).collect();
        assert_eq!(seqs, vec![0, 1]);
    }

    #[test]
    fn follow_bounding_trims_an_oversized_first_line_so_the_cursor_can_advance() {
        let mut lines = vec![line(7, RESPONSE_MAX_BYTES + 10), line(8, 1)];

        assert!(bound_follow_response_lines(&mut lines, None));

        assert_eq!(lines.len(), 1);
        assert_eq!(lines.first().map(|line| line.seq), Some(7));
        assert!(logs_fit(&lines, true, None));
        assert!(
            lines
                .first()
                .is_some_and(|line| !line.line.is_empty() && line.line.len() < RESPONSE_MAX_BYTES)
        );
    }

    #[test]
    fn log_bounding_accounts_for_json_escape_expansion() {
        let mut lines = vec![micromux::LogLine {
            seq: 1,
            run_generation: 1,
            timestamp_unix_ms: 0,
            line: "\0".repeat(RESPONSE_MAX_BYTES / 2),
        }];

        assert!(bound_follow_response_lines(&mut lines, None));
        assert!(logs_fit(&lines, true, None));
        assert_eq!(lines.first().map(|line| line.seq), Some(1));
    }

    #[tokio::test]
    async fn project_lock_blocks_a_second_endpoint_for_the_same_config() -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let config = directory.path().join("micromux.yaml");
        std::fs::write(&config, "version: 1\nservices: {}\n")?;
        let config = CanonicalConfigPath::new(config)?;
        let first = ControlEndpoint::Unix(directory.path().join("first.sock"));
        let second = ControlEndpoint::Unix(directory.path().join("second.sock"));

        let first_guard = bind_project(&first, &config)?.ok_or_else(|| eyre::eyre!("lock busy"))?;
        let ControlEndpoint::Unix(first_path) = &first else {
            eyre::bail!("test endpoint was not Unix");
        };
        for lock_path in [
            first_path.with_extension("lock"),
            project_lock_path(&config)?,
        ] {
            assert_eq!(
                std::fs::metadata(lock_path)?.permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(bind_project(&second, &config)?.is_none());
        drop(first_guard);
        assert!(bind_project(&second, &config)?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn subscription_heartbeats_and_bounds_unsolicited_client_frames() -> eyre::Result<()> {
        use std::sync::Arc;
        use std::time::Duration;

        let directory = tempfile::tempdir()?;
        let yaml = "version: 1\nservices: {}\n";
        let config_path = directory.path().join("micromux.yaml");
        std::fs::write(&config_path, yaml)?;
        let config_path = CanonicalConfigPath::new(config_path)?;
        let mut diagnostics = Vec::new();
        let config = micromux::from_str(yaml, directory.path(), 0usize, None, &mut diagnostics)?;
        let mux = Arc::new(micromux::Micromux::new(&config)?);
        let (_runner, handles) = mux.start(micromux::CancellationToken::new());
        let control = handles.service_control();
        let server = Arc::new(ControlServer::new(
            handles.reader,
            control,
            SessionIdentity::new("test".to_string(), directory.path(), &config_path),
            handles.dynamic_services,
        ));
        let (server_io, client_io) = tokio::io::duplex(4096);
        let shutdown = micromux::CancellationToken::new();
        let heartbeat_interval = Duration::from_millis(100);
        let task = tokio::spawn({
            let server = Arc::clone(&server);
            let shutdown = shutdown.clone();
            async move {
                stream_changes(
                    &server,
                    crate::framed(server_io),
                    shutdown,
                    Some(heartbeat_interval),
                )
                .await;
            }
        });
        let mut client = crate::framed(client_io);

        crate::write_message(&mut client, &Request::Describe).await?;
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            crate::read_message::<_, Response>(&mut client),
        )
        .await??
        .ok_or_else(|| eyre::eyre!("subscription closed before its heartbeat"))?;
        assert_matches!(
            response,
            Response::Change(SessionChange {
                service_id,
                kind: ChangeKind::Heartbeat,
            }) if service_id == SessionChange::SESSION_WIDE
        );
        assert!(!task.is_finished());

        // A heartbeat must not replenish the lifetime allowance for unsolicited client frames.
        let first_batch = MAX_UNSOLICITED_SUBSCRIPTION_FRAMES / 2;
        for _ in 0..first_batch {
            crate::write_message(&mut client, &Request::Describe).await?;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let response = tokio::time::timeout(
            Duration::from_secs(1),
            crate::read_message::<_, Response>(&mut client),
        )
        .await??
        .ok_or_else(|| eyre::eyre!("subscription closed before its second heartbeat"))?;
        assert_matches!(
            response,
            Response::Change(SessionChange {
                kind: ChangeKind::Heartbeat,
                ..
            })
        );

        let mut sent_frames = first_batch;
        for _ in first_batch..=MAX_UNSOLICITED_SUBSCRIPTION_FRAMES {
            match crate::write_message(&mut client, &Request::Describe).await {
                Ok(()) => sent_frames = sent_frames.saturating_add(1),
                // The peer may observe the close while its first over-budget frame is in flight.
                Err(_) if sent_frames >= MAX_UNSOLICITED_SUBSCRIPTION_FRAMES => break,
                Err(err) => return Err(err.into()),
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::timeout(Duration::from_secs(1), task).await??;

        let (server_io, client_io) = tokio::io::duplex(4096);
        let legacy_task = tokio::spawn({
            let server = Arc::clone(&server);
            let shutdown = shutdown.clone();
            async move {
                stream_changes(&server, crate::framed(server_io), shutdown, None).await;
            }
        });
        let mut legacy_client = crate::framed(client_io);
        let legacy_read = tokio::time::timeout(
            Duration::from_millis(30),
            crate::read_message::<_, Response>(&mut legacy_client),
        )
        .await;
        assert_matches!(legacy_read, Err(_));
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), legacy_task).await??;
        Ok(())
    }

    #[test]
    fn follow_page_reports_truncated_when_a_sentinel_line_was_fetched() {
        fn seq(value: usize) -> u64 {
            u64::try_from(value).unwrap_or(u64::MAX)
        }

        let lines = (0..=MAX_LOG_TAIL)
            .map(|value| line(seq(value), 1))
            .collect::<Vec<_>>();

        let (lines, truncated) = bound_follow_response_lines_page(lines, None);

        assert!(truncated);
        assert_eq!(lines.len(), MAX_LOG_TAIL);
        assert_eq!(lines.first().map(|line| line.seq), Some(0));
        assert_eq!(
            lines.last().map(|line| line.seq),
            Some(seq(MAX_LOG_TAIL.saturating_sub(1)))
        );
    }
}
