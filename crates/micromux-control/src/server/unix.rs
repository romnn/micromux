use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use micromux::{
    ChangeKind, CommandRejection, SchedulerStopped, ServiceCommandResult, SessionChange,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::sync::CancellationToken;

use super::ControlServer;
use crate::endpoint::ControlEndpoint;
use crate::protocol::{ErrorCode, PROTOCOL_VERSION, Request, Response, ServiceBrief, SessionInfo};
use crate::{
    ControlError, Framing, IDLE_TIMEOUT, REQUEST_TIMEOUT, framed, read_message, write_message,
};

/// Default number of recent log lines returned when a client does not specify `tail`.
const DEFAULT_LOG_TAIL: usize = 200;
/// Hard cap on `tail`, independent of the request frame.
const MAX_LOG_TAIL: usize = 2000;
/// Response byte budget for log/health payloads; oldest content beyond it is dropped so a chatty
/// service can't blow the frame.
const RESPONSE_MAX_BYTES: usize = 512 * 1024;

/// A bound control endpoint plus the lifetime ownership lock. Dropping it unlinks the socket while
/// the lock is still held, so a successor (which cannot acquire the lock until this process exits)
/// never has its fresh socket removed.
pub struct EndpointGuard {
    listener: tokio::net::UnixListener,
    socket_path: PathBuf,
    // Held for the whole process lifetime; the OS releases the advisory lock on exit (incl. crash).
    _lock: std::fs::File,
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
        ControlEndpoint::Unix(path) => bind_unix(path),
        ControlEndpoint::WindowsNamedPipe(_) => Err(ControlError::Unsupported),
    }
}

fn bind_unix(socket_path: &Path) -> Result<Option<EndpointGuard>, ControlError> {
    use std::os::unix::fs::PermissionsExt;

    let lock_path = socket_path.with_extension("lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;

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
    }))
}

pub(super) async fn serve(
    server: Arc<ControlServer>,
    guard: EndpointGuard,
    shutdown: CancellationToken,
) -> Result<(), ControlError> {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = guard.listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let server = Arc::clone(&server);
                        let shutdown = shutdown.clone();
                        tokio::spawn(async move {
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
                let _ = write_message(
                    &mut conn,
                    &Response::error(ErrorCode::BadRequest, err.to_string()),
                )
                .await;
                return;
            }
            // Client disconnected (None) or idle timeout (Err): close the connection.
            Ok(Ok(None)) | Err(_) => return,
        };

        if matches!(request, Request::Subscribe) {
            stream_changes(&server, conn, shutdown).await;
            return;
        }

        if matches!(request, Request::Shutdown) {
            // Acknowledge first, then cancel the shared session token: the accept loop, scheduler,
            // and TUI all observe it, so this stops the whole session (the same path as Ctrl-C).
            // Writing before cancelling ensures the client sees the ack before the endpoint vanishes.
            let _ = write_message(&mut conn, &Response::ShuttingDown).await;
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
        if write_message(&mut conn, &response).await.is_err() {
            return;
        }
    }
}

async fn stream_changes<S>(server: &ControlServer, conn: Framing<S>, shutdown: CancellationToken)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    use tokio::sync::broadcast::error::RecvError;

    let (mut sink, mut stream) = conn.split();
    let mut changes = server.reader.subscribe();
    'stream: loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            incoming = stream.next() => {
                // A subscription is one-directional from here: the client signals only via EOF
                // (None) or a transport error (Some(Err)). Either way, stop — never spin on a
                // persistent read error.
                match incoming {
                    None | Some(Err(_)) => break,
                    Some(Ok(_)) => {}
                }
            }
            change = changes.recv() => {
                match change {
                    Ok(change) => {
                        let Ok(line) = serde_json::to_string(&Response::Change(change)) else {
                            continue;
                        };
                        if sink.send(line).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(_)) => {
                        for snapshot in server.reader.services() {
                            for kind in [ChangeKind::Status, ChangeKind::Logs, ChangeKind::Health] {
                                let Ok(line) = serde_json::to_string(&Response::Change(SessionChange {
                                    service_id: snapshot.id.clone(),
                                    kind,
                                })) else {
                                    continue;
                                };
                                if sink.send(line).await.is_err() {
                                    break 'stream;
                                }
                            }
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }
}

async fn dispatch(server: &ControlServer, request: Request) -> Response {
    match request {
        Request::Describe => Response::Description(describe(server)),
        Request::ListServices => Response::Services(server.reader.services()),
        Request::GetLogs {
            service,
            run_generation,
            tail,
        } => get_logs(server, &service, run_generation, tail),
        Request::FollowLogs {
            service,
            run_generation,
            after,
        } => follow_logs(server, &service, run_generation, after),
        Request::ListLogRuns { service } => {
            if server.reader.service(&service).is_none() {
                return unknown_service(&service);
            }
            Response::LogRuns {
                runs: server.reader.log_runs(&service),
            }
        }
        Request::GetHealth { service } => {
            if server.reader.service(&service).is_none() {
                return unknown_service(&service);
            }
            let mut attempt = server.reader.latest_health(&service);
            bound_health_attempt(&mut attempt);
            Response::Health(attempt)
        }
        Request::Restart { service } => acknowledge(server.control.restart(&service).await),
        Request::RestartAll => acknowledge(server.control.restart_all().await),
        Request::Enable { service } => acknowledge(server.control.enable(&service).await),
        Request::Disable { service } => acknowledge(server.control.disable(&service).await),
        // Subscribe is intercepted before dispatch; reaching here is a protocol misuse.
        Request::Subscribe => Response::error(
            ErrorCode::BadRequest,
            "subscribe must be the only request on a connection",
        ),
        // Shutdown is intercepted before dispatch; reaching here is a protocol misuse.
        Request::Shutdown => {
            Response::error(ErrorCode::BadRequest, "shutdown is handled before dispatch")
        }
    }
}

fn get_logs(
    server: &ControlServer,
    service: &str,
    run_generation: Option<u64>,
    tail: Option<usize>,
) -> Response {
    if server.reader.service(service).is_none() {
        return unknown_service(service);
    }
    let requested_tail = tail.unwrap_or(if run_generation.is_some() {
        MAX_LOG_TAIL
    } else {
        DEFAULT_LOG_TAIL
    });
    let tail = requested_tail.min(MAX_LOG_TAIL);
    let mut truncated = requested_tail > MAX_LOG_TAIL;
    let mut lines = match run_generation {
        Some(run_generation) => {
            let Some(run) = server.reader.run_log(service, run_generation, Some(tail)) else {
                return unknown_run(service, run_generation);
            };
            run.lines
        }
        None => server.reader.logs(service, Some(tail)),
    };
    if let Some(run_generation) = run_generation
        && tail == MAX_LOG_TAIL
    {
        truncated |= server
            .reader
            .log_runs(service)
            .into_iter()
            .find(|run| run.run_generation == run_generation)
            .is_some_and(|run| run.line_count > lines.len());
    }
    truncated |= bound_tail_response_lines(&mut lines);
    Response::Logs { lines, truncated }
}

fn follow_logs(
    server: &ControlServer,
    service: &str,
    run_generation: Option<u64>,
    after: Option<u64>,
) -> Response {
    if server.reader.service(service).is_none() {
        return unknown_service(service);
    }

    // A retained disk run pages strictly forward from `after` — or from the very beginning when it
    // is omitted — keeping the oldest contiguous page, so a run longer than MAX_LOG_TAIL is fully
    // reachable by following `next_seq` rather than being pinned to its tail.
    if let Some(run_generation) = run_generation {
        let Some(run) =
            server
                .reader
                .run_log_after(service, run_generation, after, Some(MAX_LOG_TAIL))
        else {
            return unknown_run(service, run_generation);
        };
        let mut lines = run.lines;
        if let Some(cursor) = after {
            lines.retain(|line| line.seq > cursor);
        }
        let capped = lines.len() > MAX_LOG_TAIL;
        lines.truncate(MAX_LOG_TAIL);
        let truncated = capped || bound_follow_response_lines(&mut lines);
        return Response::Logs { lines, truncated };
    }

    // The bounded visible stream: page forward from a cursor, else return its most recent tail.
    let mut lines = server.reader.logs(service, None);
    if let Some(cursor) = after {
        lines.retain(|line| line.seq > cursor);
        let capped = lines.len() > MAX_LOG_TAIL;
        lines.truncate(MAX_LOG_TAIL);
        let truncated = capped || bound_follow_response_lines(&mut lines);
        return Response::Logs { lines, truncated };
    }

    let mut truncated = false;
    if lines.len() > MAX_LOG_TAIL {
        let drop = lines.len() - MAX_LOG_TAIL;
        lines.drain(0..drop);
        truncated = true;
    }
    truncated |= bound_tail_response_lines(&mut lines);
    Response::Logs { lines, truncated }
}

fn describe(server: &ControlServer) -> SessionInfo {
    let services = server
        .reader
        .services()
        .into_iter()
        .map(|snapshot| ServiceBrief {
            id: snapshot.id,
            name: snapshot.name,
        })
        .collect();
    SessionInfo {
        protocol_version: PROTOCOL_VERSION,
        id: crate::endpoint::endpoint_hash(Path::new(&server.identity.config_path)),
        pid: server.identity.pid,
        start_time: server.identity.start_time,
        name: server.identity.name.clone(),
        working_dir: server.identity.working_dir.clone(),
        config_path: server.identity.config_path.clone(),
        services,
        micromux_version: server.identity.micromux_version.clone(),
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

/// Drop oldest log lines until the payload fits the response byte budget.
fn bound_tail_response_lines(lines: &mut Vec<micromux::LogLine>) -> bool {
    let mut total: usize = lines.iter().map(|line| line.line.len()).sum();
    let mut drop_count = 0;
    for line in lines.iter() {
        if total <= RESPONSE_MAX_BYTES || lines.len().saturating_sub(drop_count) <= 1 {
            break;
        }
        total = total.saturating_sub(line.line.len());
        drop_count += 1;
    }
    if drop_count > 0 {
        lines.drain(0..drop_count);
    }
    let mut truncated = drop_count > 0;
    if total > RESPONSE_MAX_BYTES
        && let Some(line) = lines.first_mut()
    {
        line.line = trim_to_last_bytes(std::mem::take(&mut line.line), RESPONSE_MAX_BYTES);
        truncated = true;
    }
    truncated
}

/// Keep the oldest contiguous log page after a cursor, bounded by response bytes.
fn bound_follow_response_lines(lines: &mut Vec<micromux::LogLine>) -> bool {
    let mut total = 0usize;
    let mut keep = 0usize;
    for line in lines.iter_mut() {
        let line_len = line.line.len();
        if keep == 0 && line_len > RESPONSE_MAX_BYTES {
            line.line = trim_to_last_bytes(std::mem::take(&mut line.line), RESPONSE_MAX_BYTES);
            keep = 1;
            break;
        }
        if total.saturating_add(line_len) > RESPONSE_MAX_BYTES {
            break;
        }
        total += line_len;
        keep += 1;
    }
    let truncated = keep < lines.len();
    lines.truncate(keep);
    truncated
}

fn trim_to_last_bytes(line: String, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line;
    }
    if max_bytes == 0 {
        return String::new();
    }

    let min_start = line.len().saturating_sub(max_bytes);
    let start = line
        .char_indices()
        .map(|(idx, _)| idx)
        .find(|idx| *idx >= min_start)
        .unwrap_or(line.len());
    let mut line = line;
    line.split_off(start)
}

fn bound_health_attempt(attempt: &mut Option<micromux::HealthAttempt>) {
    let Some(attempt) = attempt else {
        return;
    };

    let mut total = attempt.command.len()
        + attempt
            .output
            .iter()
            .map(|line| line.line.len())
            .sum::<usize>();
    let mut drop_count = 0;
    for line in &attempt.output {
        if total <= RESPONSE_MAX_BYTES {
            break;
        }
        total = total.saturating_sub(line.line.len());
        drop_count += 1;
    }
    if drop_count > 0 {
        attempt.output.drain(0..drop_count);
    }
    if total > RESPONSE_MAX_BYTES {
        attempt.command =
            trim_to_last_bytes(std::mem::take(&mut attempt.command), RESPONSE_MAX_BYTES);
    }
}

fn acknowledge(result: Result<ServiceCommandResult, SchedulerStopped>) -> Response {
    match result {
        Ok(Ok(services)) => Response::Accepted { services },
        Ok(Err(CommandRejection::UnknownService)) => {
            Response::error(ErrorCode::UnknownService, "unknown service")
        }
        Ok(Err(CommandRejection::InvalidState)) => Response::error(
            ErrorCode::InvalidState,
            "the command is not valid in the service's current state",
        ),
        Err(SchedulerStopped) => {
            Response::error(ErrorCode::SchedulerStopped, "the scheduler has stopped")
        }
    }
}

#[cfg(test)]
mod tests {
    use micromux::LogLine;
    use similar_asserts::assert_eq;

    use super::{RESPONSE_MAX_BYTES, bound_follow_response_lines, bound_tail_response_lines};

    fn line(seq: u64, len: usize) -> LogLine {
        LogLine {
            seq,
            run_generation: 1,
            line: "x".repeat(len),
        }
    }

    #[test]
    fn tail_bounding_keeps_the_newest_lines() {
        let mut lines = vec![
            line(0, 200 * 1024),
            line(1, 200 * 1024),
            line(2, 200 * 1024),
            line(3, 200 * 1024),
        ];

        assert!(bound_tail_response_lines(&mut lines));

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

        assert!(bound_follow_response_lines(&mut lines));

        let seqs: Vec<u64> = lines.into_iter().map(|line| line.seq).collect();
        assert_eq!(seqs, vec![0, 1]);
    }

    #[test]
    fn follow_bounding_trims_an_oversized_first_line_so_the_cursor_can_advance() {
        let mut lines = vec![line(7, RESPONSE_MAX_BYTES + 10), line(8, 1)];

        assert!(bound_follow_response_lines(&mut lines));

        assert_eq!(lines.len(), 1);
        assert_eq!(lines.first().map(|line| line.seq), Some(7));
        assert_eq!(
            lines.first().map(|line| line.line.len()),
            Some(RESPONSE_MAX_BYTES)
        );
    }
}
