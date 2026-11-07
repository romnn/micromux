//! The session side: finding or starting the proxy, and keeping a registration alive.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use futures::{SinkExt as _, StreamExt as _};
use tokio::io::AsyncReadExt as _;
use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio_util::codec::{Framed, LinesCodec};
use tokio_util::sync::CancellationToken;

use crate::listen::PortPreference;
use crate::paths::ProxyPaths;
use crate::protocol::{
    ClientMessage, IDLE_TIMEOUT, LEASE_TIMEOUT, MAX_MESSAGE_BYTES, PING_INTERVAL, ProxyMessage,
    RegisteredSession, Registration, RouteTarget,
};

/// How long a session waits for a proxy it started to come up.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one request on the registration socket may take.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Longest wait between reconnection attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Longest proxy error output reported back to the session.
const MAX_ERROR_OUTPUT: u64 = 16 * 1024;

/// How long a session waits for an outdated proxy to exit before it gives up.
///
/// An outdated proxy that no session uses exits within its idle timeout.
/// The lease covers a session that went away without closing its connection, which the proxy
/// only notices once the lease runs out, and leaves room for the proxy's idle check and shutdown.
const OUTDATED_PROXY_WAIT: Duration = IDLE_TIMEOUT.saturating_add(LEASE_TIMEOUT);

/// How often a session checks whether an outdated proxy has exited.
const OUTDATED_PROXY_POLL: Duration = Duration::from_millis(250);

type Connection = Framed<UnixStream, LinesCodec>;

/// A registered connection, with the protocol its proxy speaks.
struct Live {
    connection: Connection,
    protocol: u32,
}

/// Why a session cannot use the proxy.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// No proxy answers and none could be started.
    #[error("cannot reach the micromux proxy at {}: {message}", socket.display())]
    Unreachable {
        /// The registration socket.
        socket: PathBuf,
        /// What went wrong.
        message: String,
    },
    /// A proxy was started but did not come up.
    #[error("the micromux proxy did not start: {0}")]
    StartFailed(String),
    /// The running proxy listens on a port this session does not accept.
    #[error(
        "the running micromux proxy listens on port {running}, but this session wants {wanted}; \
         stop the other sessions or align `proxy.port` / `proxy.fallback_port`"
    )]
    PortMismatch {
        /// The running proxy's port.
        running: u16,
        /// The acceptable ports, for the message.
        wanted: String,
    },
    /// The running proxy speaks an older registration protocol that would serve the routes
    /// differently, such as ignoring their paths, and other sessions still use it, or it did not
    /// exit in time.
    #[error("{}", outdated_proxy_message(*.pid, *.protocol, *.required, .sessions.as_deref()))]
    OutdatedProxy {
        /// Process id of the running proxy.
        pid: u32,
        /// The protocol the running proxy speaks.
        protocol: u32,
        /// The protocol the routes need.
        required: u32,
        /// The sessions registered with the proxy, or `None` when it cannot tell and did not
        /// exit within the wait.
        sessions: Option<Vec<RegisteredSession>>,
    },
    /// The proxy refused the registration because of a conflict with another session.
    #[error("the micromux proxy rejected the routes: {0}")]
    Rejected(String),
    /// The proxy cannot take the registration right now, such as while it shuts down.
    #[error("the micromux proxy cannot take the routes right now: {0}")]
    Unavailable(String),
    /// The proxy answered something unexpected or nothing at all.
    #[error("unexpected reply from the micromux proxy: {0}")]
    Protocol(String),
    /// The session was asked to stop while it waited for an outdated proxy to exit.
    #[error("cancelled while waiting for the outdated micromux proxy to exit")]
    Cancelled,
}

/// Explains [`ClientError::OutdatedProxy`] and what to do about it.
fn outdated_proxy_message(
    pid: u32,
    protocol: u32,
    required: u32,
    sessions: Option<&[RegisteredSession]>,
) -> String {
    let outdated = format!(
        "the running micromux proxy (pid {pid}) speaks registration protocol {protocol}, but this \
         session's routes need {required} (path routes)"
    );
    match sessions {
        Some(sessions) if !sessions.is_empty() => {
            let sessions = sessions
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{outdated}; it was started by an older micromux and still serves {sessions}: \
                 stop those sessions or restart them with this micromux, which would otherwise \
                 start it again, and start this session again; once only sessions of this \
                 micromux use it, `kill {pid}` moves them to a new proxy"
            )
        }
        _ => format!(
            "{outdated}; it was started by an older micromux and did not exit within \
             {OUTDATED_PROXY_WAIT:?}, so sessions of that micromux likely still use it: stop or \
             restart them, which would otherwise start it again, then start this session again, \
             or stop the proxy with `kill {pid}`"
        ),
    }
}

impl ClientError {
    /// Whether a retry against a freshly started proxy can succeed.
    fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Unreachable { .. } | Self::Protocol(_) | Self::Unavailable(_)
        )
    }
}

/// Starts a detached proxy process when none is running.
#[derive(Debug, Clone)]
pub struct ProxyLauncher {
    program: PathBuf,
    args: Vec<OsString>,
}

impl ProxyLauncher {
    /// A launcher running `program` with `args`, followed by `--port` and `--fallback-port`.
    #[must_use]
    pub fn new(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    fn launch(&self, preference: PortPreference) -> std::io::Result<tokio::process::Child> {
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&self.args)
            .arg("--port")
            .arg(preference.port.to_string());
        if let Some(fallback) = preference.fallback_port {
            command.arg("--fallback-port").arg(fallback.to_string());
        }
        // Its own process group keeps the proxy out of the session's terminal signals, so it
        // outlives the session that happened to start it.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(false)
            .spawn()
    }
}

/// Finds the running proxy, or starts one, and returns the port it listens on.
///
/// A running proxy is only accepted when its port is one `preference` allows.
///
/// # Errors
///
/// Returns [`ClientError::PortMismatch`] when the running proxy uses another port,
/// [`ClientError::StartFailed`] with the proxy's own explanation when a started proxy exits, and
/// [`ClientError::Unreachable`] when no proxy runs and `launcher` is `None`.
pub async fn ensure_proxy(
    paths: &ProxyPaths,
    preference: PortPreference,
    launcher: Option<&ProxyLauncher>,
) -> Result<u16, ClientError> {
    let accept = |port: u16| {
        if preference.accepts(port) {
            Ok(port)
        } else {
            Err(ClientError::PortMismatch {
                running: port,
                wanted: match preference.fallback_port {
                    Some(fallback) => format!("{} or {fallback}", preference.port),
                    None => preference.port.to_string(),
                },
            })
        }
    };
    let first = match hello(&paths.socket).await {
        Ok(port) => return accept(port),
        Err(err) => err,
    };
    let Some(launcher) = launcher else {
        return Err(first);
    };
    let mut child = launcher
        .launch(preference)
        .map_err(|err| ClientError::StartFailed(format!("cannot run the proxy: {err}")))?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Ok(port) = hello(&paths.socket).await {
            return accept(port);
        }
        if let Ok(Some(status)) = child.try_wait() {
            // It may have lost a start race to another proxy that is up by now.
            if let Ok(port) = hello(&paths.socket).await {
                return accept(port);
            }
            let mut output = String::new();
            if let Some(stderr) = child.stderr.take() {
                let _ = stderr
                    .take(MAX_ERROR_OUTPUT)
                    .read_to_string(&mut output)
                    .await;
            }
            let output = output.trim();
            return Err(ClientError::StartFailed(if output.is_empty() {
                format!("it exited with {status}")
            } else {
                output.to_string()
            }));
        }
        if Instant::now() >= deadline {
            return Err(ClientError::StartFailed(format!(
                "it did not answer on {} within {START_TIMEOUT:?}",
                paths.socket.display()
            )));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn connect(socket: &Path) -> Result<Connection, ClientError> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|err| ClientError::Unreachable {
            socket: socket.to_path_buf(),
            message: err.to_string(),
        })?;
    Ok(Framed::new(
        stream,
        LinesCodec::new_with_max_length(MAX_MESSAGE_BYTES),
    ))
}

async fn request(
    connection: &mut Connection,
    message: &ClientMessage,
) -> Result<ProxyMessage, ClientError> {
    let line =
        serde_json::to_string(message).map_err(|err| ClientError::Protocol(err.to_string()))?;
    connection
        .send(line)
        .await
        .map_err(|err| ClientError::Protocol(err.to_string()))?;
    match tokio::time::timeout(REPLY_TIMEOUT, connection.next()).await {
        Ok(Some(Ok(line))) => serde_json::from_str(&line)
            .map_err(|err| ClientError::Protocol(format!("{err}: {line}"))),
        Ok(Some(Err(err))) => Err(ClientError::Protocol(err.to_string())),
        Ok(None) => Err(ClientError::Protocol(
            "the proxy closed the connection".to_string(),
        )),
        Err(_) => Err(ClientError::Protocol(
            "the proxy did not answer".to_string(),
        )),
    }
}

async fn hello(socket: &Path) -> Result<u16, ClientError> {
    let mut connection = connect(socket).await?;
    match request(&mut connection, &ClientMessage::Hello).await? {
        ProxyMessage::Info { port, .. } => Ok(port),
        other => Err(ClientError::Protocol(format!("{other:?}"))),
    }
}

/// Connects to the proxy on exactly `port` (starting it if needed) and registers.
///
/// A proxy that speaks too old a protocol for the routes is taken over when no session uses it:
/// a proxy that answers [`ClientMessage::StopIfIdle`] is asked to stop, and any other is given
/// until [`OUTDATED_PROXY_WAIT`] to exit by itself, with `on_wait` told what the session waits
/// for.
/// Either way the next round starts a current proxy.
/// A proxy that serves other sessions, by its own account or its not-found page's, is refused
/// with their names, and nothing ever signals the old proxy's process.
async fn register(
    target: &Maintained,
    outdated_proxies: &mut OutdatedProxies,
    on_wait: &mut (dyn FnMut(&str) + Send),
    stop: &CancellationToken,
) -> Result<Live, ClientError> {
    let Maintained {
        paths,
        registration,
        port,
        launcher,
    } = target;
    // Recovery never renegotiates: the running services already carry URLs with this port.
    let exact = PortPreference {
        port: *port,
        fallback_port: None,
    };
    let required = registration.required_protocol();
    loop {
        ensure_proxy(paths, exact, launcher.as_ref()).await?;
        let mut connection = connect(&paths.socket).await?;
        // Ask on the registration connection itself, so the answer describes the very proxy that
        // takes the routes even if another one replaced it since `ensure_proxy`.
        let (proxy_port, pid, protocol, sessions) =
            match request(&mut connection, &ClientMessage::Hello).await? {
                ProxyMessage::Info {
                    port,
                    pid,
                    protocol,
                    sessions,
                } => (port, pid, protocol, sessions),
                other => return Err(ClientError::Protocol(format!("{other:?}"))),
            };
        if protocol >= required {
            return register_on(connection, registration, protocol).await;
        }
        let outdated = |sessions| ClientError::OutdatedProxy {
            pid,
            protocol,
            required,
            sessions,
        };

        // Outdated proxy: stop it if it says nobody uses it, refuse it if somebody does.
        // A proxy older than revision 3 does not say over the socket, but its not-found page
        // lists the sessions; one that tells neither way is waited out.
        let stoppable = sessions.is_some();
        let sessions = match sessions {
            Some(sessions) => Some(sessions),
            None => listed_sessions(proxy_port).await,
        };
        match sessions {
            Some(sessions) if !sessions.is_empty() => return Err(outdated(Some(sessions))),
            Some(_) if stoppable => {
                match request(&mut connection, &ClientMessage::StopIfIdle).await {
                    Ok(ProxyMessage::Stopping) => {
                        tracing::info!(pid, protocol, "stopped an idle outdated proxy");
                    }
                    Ok(ProxyMessage::Busy { sessions }) => return Err(outdated(Some(sessions))),
                    // It may be exiting already; the wait below tells.
                    Ok(_) | Err(_) => {}
                }
            }
            Some(_) | None => {}
        }

        // Wait for it to exit
        drop(connection);
        if outdated_proxies.announced != Some(pid) {
            outdated_proxies.announced = Some(pid);
            tracing::info!(
                pid,
                protocol,
                required,
                "waiting for an outdated proxy to exit"
            );
            on_wait(&format!(
                "waiting for the outdated proxy (pid {pid}) to exit"
            ));
        }
        let deadline = *outdated_proxies
            .deadline
            .get_or_insert_with(|| tokio::time::Instant::now() + OUTDATED_PROXY_WAIT);
        if tokio::time::Instant::now() >= deadline {
            return Err(outdated(None));
        }
        tokio::select! {
            () = stop.cancelled() => return Err(ClientError::Cancelled),
            () = tokio::time::sleep(OUTDATED_PROXY_POLL) => {}
        }
    }
}

/// The wait for outdated proxies to exit, which spans every registration attempt until one
/// succeeds.
///
/// One bound covers them all, even when the attempt fails for another reason in between or an
/// older micromux starts another outdated proxy after the first one exits.
#[derive(Debug, Default)]
struct OutdatedProxies {
    /// When the session stops waiting, set once it first finds an outdated proxy.
    deadline: Option<tokio::time::Instant>,
    /// The outdated proxy the session last reported waiting for.
    announced: Option<u32>,
}

impl OutdatedProxies {
    /// Whether the session is still within its wait for an outdated proxy.
    fn waiting(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| tokio::time::Instant::now() < deadline)
    }
}

/// The host the session asks a proxy's not-found page for; no session domain contains it.
const PROBE_HOST: &str = "micromux-probe.invalid";

/// The sessions an older proxy listening on `port` lists on its not-found page, or `None` when
/// the page cannot be read.
///
/// Every proxy answers a host no session serves with a page that says either that no session is
/// registered or which sessions are, so a proxy that predates [`ProxyMessage::Info`]'s session
/// list still tells.
async fn listed_sessions(port: u16) -> Option<Vec<RegisteredSession>> {
    let probe = async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .ok()?;
        tokio::io::AsyncWriteExt::write_all(
            &mut stream,
            format!("GET / HTTP/1.1\r\nHost: {PROBE_HOST}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .ok()?;
        let mut response = String::new();
        stream
            .take(MAX_PAGE_BYTES)
            .read_to_string(&mut response)
            .await
            .ok()?;
        Some(response)
    };
    let response = tokio::time::timeout(REPLY_TIMEOUT, probe).await.ok()??;
    parse_not_found_sessions(&response)
}

/// Longest not-found page read from an older proxy.
const MAX_PAGE_BYTES: u64 = 256 * 1024;

/// Reads the session list off a proxy's not-found response.
fn parse_not_found_sessions(response: &str) -> Option<Vec<RegisteredSession>> {
    if !response.starts_with("HTTP/1.1 404 ") {
        return None;
    }
    if response.contains("<p>No session is registered with the proxy right now.</p>") {
        return Some(Vec::new());
    }
    let (_, list) = response.split_once("<p>Live sessions:</p>")?;
    // The page escapes names and paths, so neither contains the markup around them.
    let mut sessions = Vec::new();
    for item in list.split("<li><strong>").skip(1) {
        let (session, rest) = item.split_once("</strong> <small>(")?;
        let (config_path, _) = rest.split_once(")</small>")?;
        sessions.push(RegisteredSession {
            session: unescape_html(session),
            config_path: unescape_html(config_path),
        });
    }
    (!sessions.is_empty()).then_some(sessions)
}

/// Reverses the proxy pages' HTML escaping.
fn unescape_html(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// Registers on `connection`, whose proxy speaks `protocol`.
async fn register_on(
    mut connection: Connection,
    registration: &Registration,
    protocol: u32,
) -> Result<Live, ClientError> {
    match request(
        &mut connection,
        &ClientMessage::Register(registration.clone()),
    )
    .await?
    {
        ProxyMessage::Registered => Ok(Live {
            connection,
            protocol,
        }),
        ProxyMessage::Rejected {
            reason,
            transient: true,
        } => Err(ClientError::Unavailable(reason)),
        ProxyMessage::Rejected {
            reason,
            transient: false,
        } => Err(ClientError::Rejected(reason)),
        other => Err(ClientError::Protocol(format!("{other:?}"))),
    }
}

/// Whether a session's routes are currently served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationStatus {
    /// The proxy serves the routes.
    Registered,
    /// The routes are not served right now; the registrar keeps retrying.
    Unavailable {
        /// Why, for display.
        reason: String,
    },
}

/// Keeps one session's routes registered for as long as it runs.
///
/// The registration follows the connection: when the proxy goes away, the registrar starts it
/// again on the same port and re-registers; it never moves the session to another port.
#[derive(Debug)]
pub struct Registrar {
    status: watch::Receiver<RegistrationStatus>,
    routes: watch::Sender<Vec<RouteTarget>>,
    stop: CancellationToken,
}

impl Registrar {
    /// Registers `registration` with the proxy listening on `port`, then keeps it alive until
    /// `shutdown` fires or the registrar is dropped.
    ///
    /// The registration's `proxy_port` is set to `port`, so a proxy on any other port refuses it.
    ///
    /// A running proxy that speaks too old a protocol for the routes but serves no session is
    /// taken over: the registrar has it stop, or waits for it to exit by itself, and then starts
    /// a current one.
    /// `on_wait` receives a description of such a wait, such as `waiting for the outdated proxy
    /// (pid 4242) to exit`, for the session to show while it starts.
    /// Later waits, on a reconnect, are reported through [`Self::status`].
    ///
    /// # Errors
    ///
    /// Returns the first registration's error, such as [`ClientError::Rejected`] when another
    /// live session already serves the same session domain,
    /// [`ClientError::OutdatedProxy`] when an outdated proxy still serves other sessions or does
    /// not exit in time, and [`ClientError::Cancelled`] when `shutdown` fires during that wait.
    pub async fn start(
        paths: ProxyPaths,
        registration: Registration,
        port: u16,
        launcher: Option<ProxyLauncher>,
        shutdown: &CancellationToken,
        mut on_wait: impl FnMut(&str) + Send,
    ) -> Result<Self, ClientError> {
        let registration = Registration {
            proxy_port: port,
            ..registration
        };
        let maintained = Maintained {
            paths,
            registration,
            port,
            launcher,
        };
        // A proxy that answered a moment ago may be exiting for idleness or replaced by now, so
        // retry what a fresh proxy can fix; conflicts and port mismatches stay fatal.
        let deadline = Instant::now() + START_TIMEOUT;
        let mut backoff = Duration::from_millis(50);
        let mut outdated_proxies = OutdatedProxies::default();
        let connection = loop {
            match register(&maintained, &mut outdated_proxies, &mut on_wait, shutdown).await {
                Ok(connection) => break connection,
                // An outdated proxy that exits while the session waits for it can fail an attempt
                // midway, which the next attempt recovers from as long as the wait lasts.
                Err(err)
                    if err.is_transient()
                        && (Instant::now() < deadline || outdated_proxies.waiting()) =>
                {
                    tracing::debug!(%err, "retrying the proxy registration");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(1));
                }
                Err(err) => return Err(err),
            }
        };
        let (status_tx, status) = watch::channel(RegistrationStatus::Registered);
        let (routes, routes_rx) = watch::channel(maintained.registration.routes.clone());
        let stop = shutdown.child_token();
        tokio::spawn(maintain(
            maintained,
            connection,
            routes_rx,
            status_tx,
            stop.clone(),
        ));
        Ok(Self {
            status,
            routes,
            stop,
        })
    }

    /// Follows the registration's status.
    #[must_use]
    pub fn status(&self) -> watch::Receiver<RegistrationStatus> {
        self.status.clone()
    }

    /// A handle that replaces the registered routes, such as after the session's config changed
    /// which service owns a route.
    ///
    /// The registrar re-registers on its live connection, so the proxy swaps the routes without
    /// dropping them in between, and every later reconnect registers the latest routes.
    #[must_use]
    pub fn routes(&self) -> watch::Sender<Vec<RouteTarget>> {
        self.routes.clone()
    }
}

impl Drop for Registrar {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// What a registrar registers, and where.
struct Maintained {
    paths: ProxyPaths,
    registration: Registration,
    port: u16,
    launcher: Option<ProxyLauncher>,
}

async fn maintain(
    mut maintained: Maintained,
    connection: Live,
    mut routes: watch::Receiver<Vec<RouteTarget>>,
    status: watch::Sender<RegistrationStatus>,
    stop: CancellationToken,
) {
    let mut connection = Some(connection);
    let mut backoff = Duration::from_millis(250);
    let mut outdated_proxies = OutdatedProxies::default();
    loop {
        if let Some(live) = connection.take() {
            outdated_proxies = OutdatedProxies::default();
            // The channel starts out registered, so only a restored registration notifies.
            status.send_if_modified(|current| {
                let restored = *current != RegistrationStatus::Registered;
                *current = RegistrationStatus::Registered;
                restored
            });
            backoff = Duration::from_millis(250);
            let Some(reason) = keep_alive(live, &mut maintained, &mut routes, &stop).await else {
                return;
            };
            tracing::warn!(reason, "lost the proxy registration; reconnecting");
            status.send_replace(RegistrationStatus::Unavailable { reason });
        }
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(backoff) => {}
        }
        maintained.registration.routes = routes.borrow_and_update().clone();
        let mut on_wait = |reason: &str| {
            status.send_replace(RegistrationStatus::Unavailable {
                reason: reason.to_string(),
            });
        };
        match register(&maintained, &mut outdated_proxies, &mut on_wait, &stop).await {
            Ok(live) => connection = Some(live),
            Err(ClientError::Cancelled) => return,
            Err(err) => {
                tracing::warn!(%err, "cannot restore the proxy registration");
                status.send_replace(RegistrationStatus::Unavailable {
                    reason: err.to_string(),
                });
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// Pings until the connection is lost, returning why, or `None` once `stop` fires.
///
/// New routes are registered on the same connection, which the proxy treats as a replacement.
/// Its reply refreshes the lease like a pong; a refusal closes the connection, which reconnects.
/// New routes that need a newer protocol than the proxy speaks end the connection instead, so the
/// reconnect checks them against whichever proxy runs then.
async fn keep_alive(
    live: Live,
    maintained: &mut Maintained,
    routes: &mut watch::Receiver<Vec<RouteTarget>>,
    stop: &CancellationToken,
) -> Option<String> {
    let Live {
        mut connection,
        protocol,
    } = live;
    let ping = serde_json::to_string(&ClientMessage::Ping).unwrap_or_default();
    let mut ticker = tokio::time::interval(PING_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut deadline = tokio::time::Instant::now() + LEASE_TIMEOUT;
    loop {
        tokio::select! {
            () = stop.cancelled() => return None,
            _ = ticker.tick() => {
                if connection.send(ping.clone()).await.is_err() {
                    return Some("the proxy connection broke".to_string());
                }
            }
            changed = routes.changed() => {
                // The registrar holds a sender for as long as it lives, and dropping it stops
                // the registration too.
                if changed.is_err() {
                    return None;
                }
                maintained.registration.routes = routes.borrow_and_update().clone();
                if maintained.registration.required_protocol() > protocol {
                    return Some("the new routes need a newer proxy".to_string());
                }
                let register = ClientMessage::Register(maintained.registration.clone());
                let Ok(register) = serde_json::to_string(&register) else {
                    return Some("cannot encode the new routes".to_string());
                };
                if connection.send(register).await.is_err() {
                    return Some("the proxy connection broke".to_string());
                }
            }
            message = connection.next() => match message {
                Some(Ok(_)) => deadline = tokio::time::Instant::now() + LEASE_TIMEOUT,
                Some(Err(err)) => return Some(err.to_string()),
                None => return Some("the proxy closed the connection".to_string()),
            },
            () = tokio::time::sleep_until(deadline) => {
                return Some("the proxy stopped answering".to_string());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use similar_asserts::assert_eq;

    /// A protocol 1 proxy's not-found page, as it renders it, lists the sessions with their
    /// escaped names and config paths, or says that none is registered.
    #[test]
    fn reads_the_sessions_off_a_not_found_page() {
        let response = |body: &str| {
            format!(
                "HTTP/1.1 404 Not Found\r\ncontent-type: text/html; charset=utf-8\r\n\r\n\
                 <!doctype html><html><head><meta charset=\"utf-8\"><title>404 Not Found: no \
                 micromux route</title></head><body><h1>404 Not Found: no micromux route</h1>\
                 <p>No micromux session serves <code>micromux-probe.invalid</code>.</p>{body}\
                 <hr><small>micromux proxy</small></body></html>"
            )
        };

        // Idle
        assert_eq!(
            parse_not_found_sessions(&response(
                "<p>No session is registered with the proxy right now.</p>"
            )),
            Some(Vec::new())
        );

        // Busy, with escaped names and paths
        let busy = response(
            "<p>Live sessions:</p><ul>\
             <li><strong>luup2</strong> <small>(/home/me/luup2/micromux.yaml)</small><ul>\
             <li><a href=\"http://app.luup2.localhost\">http://app.luup2.localhost</a></li>\
             </ul></li>\
             <li><strong>a&amp;b</strong> <small>(/tmp/&lt;x&gt; &#39;y&#39;/micromux.yaml)</small>\
             <ul></ul></li></ul>",
        );
        assert_eq!(
            parse_not_found_sessions(&busy),
            Some(vec![
                RegisteredSession {
                    session: "luup2".to_string(),
                    config_path: "/home/me/luup2/micromux.yaml".to_string(),
                },
                RegisteredSession {
                    session: "a&b".to_string(),
                    config_path: "/tmp/<x> 'y'/micromux.yaml".to_string(),
                },
            ])
        );

        // Anything else tells nothing
        assert_eq!(
            parse_not_found_sessions("HTTP/1.1 502 Bad Gateway\r\n\r\n"),
            None
        );
        assert_eq!(
            parse_not_found_sessions(&response("<p>Something else</p>")),
            None
        );
    }
}
