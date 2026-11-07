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
    ClientMessage, LEASE_TIMEOUT, MAX_MESSAGE_BYTES, PING_INTERVAL, ProxyMessage, Registration,
    RouteTarget,
};

/// How long a session waits for a proxy it started to come up.
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
    /// differently, such as ignoring their paths.
    #[error(
        "the running micromux proxy (pid {pid}) speaks registration protocol {protocol}, but this \
         session's routes need {required} (path routes); it was started by an older micromux, \
         so stop or restart the sessions still running that micromux, which would start it \
         again, then wait a minute for it to exit or stop it with `kill {pid}`, and start this \
         session again"
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
async fn register(
    paths: &ProxyPaths,
    registration: &Registration,
    port: u16,
    launcher: Option<&ProxyLauncher>,
) -> Result<Live, ClientError> {
    // Recovery never renegotiates: the running services already carry URLs with this port.
    let exact = PortPreference {
        port,
        fallback_port: None,
    };
    ensure_proxy(paths, exact, launcher).await?;
    let mut connection = connect(&paths.socket).await?;
    // Ask on the registration connection itself, so the answer describes the very proxy that
    // takes the routes even if another one replaced it since `ensure_proxy`.
    let protocol = match request(&mut connection, &ClientMessage::Hello).await? {
        ProxyMessage::Info { pid, protocol, .. } if protocol < registration.required_protocol() => {
            return Err(ClientError::OutdatedProxy {
                pid,
                protocol,
                required: registration.required_protocol(),
            });
        }
        ProxyMessage::Info { protocol, .. } => protocol,
        other => return Err(ClientError::Protocol(format!("{other:?}"))),
    };
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
    /// # Errors
    ///
    /// Returns the first registration's error, such as [`ClientError::Rejected`] when another
    /// live session already serves the same session domain.
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
        // A proxy that answered a moment ago may be exiting for idleness or replaced by now, so
        // retry what a fresh proxy can fix; conflicts and port mismatches stay fatal.
        let deadline = Instant::now() + START_TIMEOUT;
        let mut backoff = Duration::from_millis(50);
        let connection = loop {
            match register(&paths, &registration, port, launcher.as_ref()).await {
                Ok(connection) => break connection,
                Err(err) if err.is_transient() && Instant::now() < deadline => {
                    tracing::debug!(%err, "retrying the proxy registration");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(1));
                }
                Err(err) => return Err(err),
            }
        };
        let (status_tx, status) = watch::channel(RegistrationStatus::Registered);
        let (routes, routes_rx) = watch::channel(registration.routes.clone());
        let stop = shutdown.child_token();
        tokio::spawn(maintain(
            Maintained {
                paths,
                registration,
                port,
                launcher,
            },
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
        match register(
            &maintained.paths,
            &maintained.registration,
            maintained.port,
            maintained.launcher.as_ref(),
        )
        .await
        {
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
