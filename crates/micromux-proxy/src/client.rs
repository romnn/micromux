//! The session side: finding, starting or replacing the proxy, and keeping a registration alive.

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

use crate::listen::{self, PortPreference};
use crate::paths::ProxyPaths;
use crate::protocol::{
    ClientMessage, LEASE_TIMEOUT, MAX_MESSAGE_BYTES, PING_INTERVAL, PORTS_PROTOCOL, ProxyMessage,
    Registration, RouteTarget,
};

/// How long a session waits for a proxy it started to come up, which also bounds a replacement.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one request on the registration socket may take.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Longest wait between reconnection attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Longest proxy error output reported back to the session.
const MAX_ERROR_OUTPUT: u64 = 16 * 1024;

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
    /// The running proxy cannot listen on any port this session accepts.
    #[error("the micromux proxy cannot serve this session's port: {0}")]
    CannotListen(String),
    /// The running proxy is too old to listen on more than one port, and its port is not one
    /// this session accepts.
    #[error(
        "the running micromux proxy was started by an older micromux and listens only on port \
         {running}, but this session wants {wanted}; it exits a minute after the sessions using \
         it stop, or align `proxy.port` / `proxy.fallback_port` with it"
    )]
    PortMismatch {
        /// The running proxy's port.
        running: u16,
        /// The acceptable ports, for the message.
        wanted: String,
    },
    /// The running proxy speaks an older registration protocol that would serve the routes
    /// differently, such as ignoring their paths, and this session could not replace it.
    #[error(
        "the running micromux proxy (pid {pid}) speaks registration protocol {protocol}, but this \
         session's routes need {required} (path routes); it was started by an older micromux, \
         which starts it again while its sessions run: restart them with this micromux, or stop \
         the proxy with `kill {pid}`"
    )]
    OutdatedProxy {
        /// Process id of the running proxy.
        pid: u32,
        /// The protocol the running proxy speaks.
        protocol: u32,
        /// The protocol the routes need.
        required: u32,
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

/// Starts a detached proxy process when none is running, or in place of one from an older
/// release.
#[derive(Debug, Clone)]
pub struct ProxyLauncher {
    program: PathBuf,
    args: Vec<OsString>,
    /// The micromux release `program` runs, or `None` when it does not parse, which never
    /// replaces a running proxy.
    version: Option<semver::Version>,
}

impl ProxyLauncher {
    /// A launcher running `program`, of micromux release `version`, with `args`, followed by
    /// `--port` and `--fallback-port`.
    #[must_use]
    pub fn new(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
        version: &str,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            version: semver::Version::parse(version).ok(),
        }
    }

    /// Whether a proxy that reports `version` runs an older release than this launcher starts.
    ///
    /// A proxy that reports none predates the report, so it is older; one whose version does not
    /// parse is never taken for older.
    fn upgrades(&self, version: Option<&str>) -> bool {
        let Some(ours) = &self.version else {
            return false;
        };
        version.is_none_or(|theirs| {
            semver::Version::parse(theirs).is_ok_and(|theirs| theirs.cmp_precedence(ours).is_lt())
        })
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

/// What a proxy says about itself in [`ProxyMessage::Info`].
#[derive(Debug)]
struct ProxyInfo {
    port: u16,
    pid: u32,
    protocol: u32,
    version: Option<String>,
}

/// One attempt's budget for finding, starting or replacing the proxy.
struct Handoff {
    deadline: Instant,
    /// The proxy this attempt asked to exit, which may still answer while it shuts down.
    ///
    /// An attempt replaces at most one proxy, so sessions of different releases cannot keep
    /// replacing each other's proxies within it.
    retired: Option<u32>,
}

impl Handoff {
    fn new() -> Self {
        Self {
            deadline: Instant::now() + START_TIMEOUT,
            retired: None,
        }
    }
}

/// Finds the running proxy, or starts one, has it listen as `preference` asks, and returns the
/// port it serves the session on.
///
/// A running proxy listens on the preferred port, or on the fallback when the preferred one is
/// privileged or taken, alongside the ports it serves other sessions on.
/// A running proxy from an older release than `launcher` starts is replaced first: a successor
/// starts on its port and the old proxy exits, which drops the connections it carried, and every
/// session registered with it registers again with the successor.
/// A proxy older than [`PORTS_PROTOCOL`] listens on one port only, and is accepted only when
/// that port is one `preference` allows.
///
/// # Errors
///
/// Returns [`ClientError::CannotListen`] when the proxy cannot bind an acceptable port,
/// [`ClientError::PortMismatch`] when an older proxy that cannot be replaced listens on another
/// port, [`ClientError::StartFailed`] with the proxy's own explanation when a started proxy
/// exits, and [`ClientError::Unreachable`] when no proxy runs and `launcher` is `None`.
pub async fn ensure_proxy(
    paths: &ProxyPaths,
    preference: PortPreference,
    launcher: Option<&ProxyLauncher>,
) -> Result<u16, ClientError> {
    let mut handoff = Handoff::new();
    loop {
        let (connection, info) = connect_current(paths, preference, launcher, &mut handoff).await?;
        match listen(connection, &info, preference).await {
            // A proxy that answered a moment ago may be exiting for idleness by now.
            Err(err)
                if err.is_transient()
                    && launcher.is_some()
                    && Instant::now() < handoff.deadline =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            result => return result,
        }
    }
}

/// Connects to the running proxy, and returns the connection with what the proxy said about
/// itself.
///
/// Without a running proxy, `launcher` starts one that listens as `preference` asks.
/// A running proxy from an older release than `launcher` starts is replaced (see [`replace`]),
/// once per `handoff`; a session without a launcher uses whichever proxy runs.
async fn connect_current(
    paths: &ProxyPaths,
    preference: PortPreference,
    launcher: Option<&ProxyLauncher>,
    handoff: &mut Handoff,
) -> Result<(Connection, ProxyInfo), ClientError> {
    loop {
        let started = match hello(&paths.socket).await {
            // The replaced proxy may still answer while it shuts down.
            Ok((_, info)) if Some(info.pid) == handoff.retired => None,
            Ok((connection, info)) => {
                let Some(launcher) = launcher.filter(|launcher| {
                    handoff.retired.is_none() && launcher.upgrades(info.version.as_deref())
                }) else {
                    return Ok((connection, info));
                };
                match replace(&connection, &info, launcher) {
                    Ok(successor) => {
                        tracing::info!(
                            pid = info.pid,
                            version = info.version.as_deref().unwrap_or("unknown"),
                            "replacing a proxy from an older micromux"
                        );
                        handoff.retired = Some(info.pid);
                        Some(successor)
                    }
                    Err(reason) => {
                        tracing::info!(
                            pid = info.pid,
                            reason,
                            "keeping a proxy from an older micromux"
                        );
                        return Ok((connection, info));
                    }
                }
            }
            Err(err) if err.is_transient() => {
                let Some(launcher) = launcher else {
                    return Err(err);
                };
                Some(launcher.launch(preference).map_err(|err| {
                    ClientError::StartFailed(format!("cannot run the proxy: {err}"))
                })?)
            }
            Err(err) => return Err(err),
        };
        match started {
            Some(child) => wait_for_start(paths, child, handoff).await?,
            None if Instant::now() >= handoff.deadline => return Err(no_answer(paths)),
            None => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

/// Waits until a proxy other than the one `handoff` retired answers, which `child`, a proxy this
/// session started, may or may not be.
///
/// A proxy that wins the start instead of `child` leaves it waiting for the lock, so `child` is
/// stopped then: taking over later, it could listen on a port older sessions do not accept.
async fn wait_for_start(
    paths: &ProxyPaths,
    mut child: tokio::process::Child,
    handoff: &Handoff,
) -> Result<(), ClientError> {
    let answered = async || {
        hello(&paths.socket)
            .await
            .ok()
            .map(|(_, info)| info.pid)
            .filter(|pid| Some(*pid) != handoff.retired)
    };
    loop {
        if let Some(pid) = answered().await {
            if child.id() != Some(pid) {
                let _ = child.start_kill();
            }
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            // It may have lost a start race to another proxy that is up by now.
            if answered().await.is_some() {
                return Ok(());
            }
            return Err(start_failed(&mut child, status).await);
        }
        if Instant::now() >= handoff.deadline {
            return Err(no_answer(paths));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn no_answer(paths: &ProxyPaths) -> ClientError {
    ClientError::StartFailed(format!(
        "no current proxy answered on {} within {START_TIMEOUT:?}",
        paths.socket.display()
    ))
}

/// Explains why a proxy this session started exited with `status`, from its own error output.
async fn start_failed(
    child: &mut tokio::process::Child,
    status: std::process::ExitStatus,
) -> ClientError {
    let mut output = String::new();
    if let Some(stderr) = child.stderr.take() {
        let _ = stderr
            .take(MAX_ERROR_OUTPUT)
            .read_to_string(&mut output)
            .await;
    }
    let output = output.trim();
    ClientError::StartFailed(if output.is_empty() {
        format!("it exited with {status}")
    } else {
        output.to_string()
    })
}

/// Starts a successor to the proxy behind `connection`, which `info` describes, and asks that
/// proxy to exit.
///
/// The successor starts on the proxy's own port, which sessions of every release accept, and
/// waits for the proxy's lock, so it takes over as soon as the proxy lets go, before a session of
/// an older release can start one of its own.
/// The signal goes only to the process verified as the socket's peer, run by this user, and never
/// to this process.
/// Its pid could still be reused between the check and the signal, but only if the proxy exits
/// on its own in that moment.
///
/// Returns why the proxy is kept instead, such as when the successor could not listen on the
/// proxy's port because it is privileged and the older binary had the capability to bind it.
fn replace(
    connection: &Connection,
    info: &ProxyInfo,
    launcher: &ProxyLauncher,
) -> Result<tokio::process::Child, String> {
    let peer = connection
        .get_ref()
        .peer_cred()
        .map_err(|err| format!("cannot identify the proxy process: {err}"))?;
    let pid = i32::try_from(info.pid).map_err(|_| format!("invalid pid {}", info.pid))?;
    if peer.pid() != Some(pid)
        || peer.uid() != nix::unistd::geteuid().as_raw()
        || info.pid == std::process::id()
    {
        return Err("the process behind the socket is not the proxy it reports".to_string());
    }
    if !listen::may_bind(info.port) {
        return Err(format!(
            "this micromux may not listen on port {}",
            info.port
        ));
    }
    let mut successor = launcher
        .launch(PortPreference {
            port: info.port,
            fallback_port: None,
        })
        .map_err(|err| format!("cannot run the proxy: {err}"))?;
    match nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGTERM,
    ) {
        // Another session replaced it first, and the successor may already be taking over.
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(successor),
        Err(err) => {
            let _ = successor.start_kill();
            Err(format!("cannot stop it: {err}"))
        }
    }
}

/// Has the proxy behind `connection`, which `info` describes, listen as `preference` asks, and
/// returns the port.
async fn listen(
    mut connection: Connection,
    info: &ProxyInfo,
    preference: PortPreference,
) -> Result<u16, ClientError> {
    if info.protocol < PORTS_PROTOCOL {
        if preference.accepts(info.port) {
            return Ok(info.port);
        }
        return Err(ClientError::PortMismatch {
            running: info.port,
            wanted: match preference.fallback_port {
                Some(fallback) => format!("{} or {fallback}", preference.port),
                None => preference.port.to_string(),
            },
        });
    }
    let listen = ClientMessage::Listen {
        port: preference.port,
        fallback_port: preference.fallback_port,
    };
    match request(&mut connection, &listen).await? {
        ProxyMessage::Listening { port } => Ok(port),
        ProxyMessage::Rejected {
            reason,
            transient: true,
        } => Err(ClientError::Unavailable(reason)),
        ProxyMessage::Rejected {
            reason,
            transient: false,
        } => Err(ClientError::CannotListen(reason)),
        other => Err(ClientError::Protocol(format!("{other:?}"))),
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

/// Connects to the proxy answering on `socket` and asks what it is.
async fn hello(socket: &Path) -> Result<(Connection, ProxyInfo), ClientError> {
    let mut connection = connect(socket).await?;
    match request(&mut connection, &ClientMessage::Hello).await? {
        ProxyMessage::Info {
            port,
            pid,
            protocol,
            version,
            ..
        } => Ok((
            connection,
            ProxyInfo {
                port,
                pid,
                protocol,
                version,
            },
        )),
        other => Err(ClientError::Protocol(format!("{other:?}"))),
    }
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

/// Connects to the proxy serving exactly `port` (starting or replacing it as
/// [`connect_current`] does) and registers.
///
/// A proxy from revision 4 on opens the port for the registration itself.
async fn register(target: &Maintained, handoff: &mut Handoff) -> Result<Live, ClientError> {
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
    let (connection, info) = connect_current(paths, exact, launcher.as_ref(), handoff).await?;
    if info.protocol < PORTS_PROTOCOL && info.port != *port {
        return Err(ClientError::PortMismatch {
            running: info.port,
            wanted: port.to_string(),
        });
    }
    let required = registration.required_protocol();
    if info.protocol < required {
        return Err(ClientError::OutdatedProxy {
            pid: info.pid,
            protocol: info.protocol,
            required,
        });
    }
    register_on(connection, registration, info.protocol).await
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
    /// The registration's `proxy_port` is set to `port`, the only port the proxy serves its
    /// routes on; a proxy that does not listen on it opens it, or refuses the registration as
    /// retryable while it cannot.
    ///
    /// A running proxy from an older release than `launcher` starts is replaced first, as
    /// [`ensure_proxy`] describes.
    ///
    /// # Errors
    ///
    /// Returns the first registration's error, such as [`ClientError::Rejected`] when another
    /// live session already serves the same session domain, and [`ClientError::OutdatedProxy`]
    /// when a proxy too old for the routes runs and cannot be replaced.
    pub async fn start(
        paths: ProxyPaths,
        registration: Registration,
        port: u16,
        launcher: Option<ProxyLauncher>,
        shutdown: &CancellationToken,
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
        let mut handoff = Handoff::new();
        let mut backoff = Duration::from_millis(50);
        let connection = loop {
            match register(&maintained, &mut handoff).await {
                Ok(connection) => break connection,
                Err(err) if err.is_transient() && Instant::now() < handoff.deadline => {
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
    loop {
        if let Some(live) = connection.take() {
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
        match register(&maintained, &mut Handoff::new()).await {
            Ok(live) => connection = Some(live),
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
