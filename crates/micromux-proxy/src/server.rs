//! The proxy process: registration socket, route table, HTTP listeners, and idle exit.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use futures::{SinkExt as _, StreamExt as _};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::codec::{Framed, LinesCodec};
use tokio_util::sync::CancellationToken;

use crate::listen::{self, Acquired, BindError, Bound, PortPreference};
use crate::pages::ListedSession;
use crate::paths::ProxyPaths;
use crate::protocol::{
    ClientMessage, LEASE_TIMEOUT, MAX_MESSAGE_BYTES, PROTOCOL_VERSION, ProxyMessage,
    RegisteredSession, Registration, RouteTarget,
};

/// How long a starting proxy waits for a previous one to release the singleton lock.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// How the proxy runs.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// Lock and socket locations.
    pub paths: ProxyPaths,
    /// Which port to listen on first; sessions can open more.
    pub preference: PortPreference,
    /// How long the proxy keeps running without any registered session.
    pub idle_timeout: Duration,
}

/// Why the proxy could not run.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// Another proxy holds the singleton lock.
    #[error("another micromux proxy is already running (it holds {})", lock.display())]
    AlreadyRunning {
        /// The contested lock file.
        lock: PathBuf,
    },
    /// The lock or socket file could not be prepared.
    #[error("cannot prepare {}: {source}", path.display())]
    Io {
        /// The file that failed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The HTTP listeners could not be bound.
    #[error(transparent)]
    Bind(#[from] BindError),
}

/// Runs the proxy until it has had no registered session for `idle_timeout`, or `shutdown` fires.
///
/// `on_ready` receives the effective port once the listeners and the registration socket are up.
/// The proxy also listens on every port its sessions ask for, and stops listening on a port once
/// no session has been registered on it for `idle_timeout`.
///
/// # Errors
///
/// Returns [`ServeError::AlreadyRunning`] when another proxy runs, [`ServeError::Bind`] when no
/// acceptable port can be bound, and [`ServeError::Io`] when the lock or socket fails.
pub async fn serve(
    options: ServeOptions,
    shutdown: CancellationToken,
    on_ready: impl FnOnce(u16),
) -> Result<(), ServeError> {
    let ServeOptions {
        paths,
        preference,
        idle_timeout,
    } = options;
    let io = |path: &PathBuf| {
        let path = path.clone();
        move |source| ServeError::Io { path, source }
    };

    // Become the one proxy of this user
    let lock = acquire_lock(&paths.lock).await?;

    // Bind every listener before announcing the socket, so a session that reaches the socket can
    // rely on the port
    let bound = listen::bind(preference)?;
    let port = bound.port;
    match std::fs::remove_file(&paths.socket) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(io(&paths.socket)(err)),
    }
    let registrations = UnixListener::bind(&paths.socket).map_err(io(&paths.socket))?;

    let stop = shutdown.child_token();
    let shared = Arc::new(Shared::new(port, stop.clone()));
    shared.open(&mut *shared.ports.lock().await, bound)?;
    let registration_acceptor = tokio::spawn(accept_registrations(
        registrations,
        shared.clone(),
        stop.clone(),
    ));
    tracing::info!(port, "micromux proxy listening");
    on_ready(port);

    // Run until idle or told to stop
    let tick = (idle_timeout / 4).clamp(Duration::from_millis(50), Duration::from_secs(1));
    loop {
        tokio::select! {
            () = stop.cancelled() => break,
            () = tokio::time::sleep(tick) => {
                if shared.try_begin_idle_exit(idle_timeout) {
                    tracing::info!("no session registered; exiting");
                    break;
                }
                shared.close_idle_ports(idle_timeout).await;
            }
        }
    }

    // End every registration and connection, then unpublish the socket while still holding the
    // lock, so a session that finds no socket can start a replacement without racing this one.
    stop.cancel();
    shared.end_all_registrations();
    let _ = std::fs::remove_file(&paths.socket);
    // The acceptors own the listeners; a port is only free for a successor once they are gone.
    let ports = std::mem::take(&mut *shared.ports.lock().await);
    for (_, port) in ports {
        port.close().await;
    }
    if let Err(err) = registration_acceptor.await {
        tracing::warn!(%err, "the registration acceptor failed");
    }
    drop(lock);
    Ok(())
}

async fn acquire_lock(path: &PathBuf) -> Result<std::fs::File, ServeError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|source| ServeError::Io {
            path: path.clone(),
            source,
        })?;
    // A proxy that just decided to exit releases the lock right after unpublishing its socket.
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(ServeError::AlreadyRunning { lock: path.clone() });
            }
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(ServeError::Io {
                    path: path.clone(),
                    source,
                });
            }
        }
    }
}

/// The route table, the listeners, and the lifecycle state shared by every connection.
pub(crate) struct Shared {
    /// The port the proxy started with, which [`ProxyMessage::Info`] reports.
    started_port: u16,
    state: Mutex<State>,
    /// The HTTP listeners by port.
    ///
    /// Opening a port for a registration holds this lock until the registration is in `state`,
    /// and closing an idle port holds it while it checks `state`, so a port never closes under a
    /// registration that just opened it.
    /// Whoever takes both locks takes this one first.
    ports: tokio::sync::Mutex<HashMap<u16, Port>>,
    /// Cancelled when the proxy stops, which stops every port's acceptors.
    stop: CancellationToken,
}

/// The listeners of one port.
struct Port {
    /// When a session last was registered on the port, or asked for it.
    last_used: tokio::time::Instant,
    /// Stops this port's acceptors.
    stop: CancellationToken,
    /// The tasks that own the port's listeners.
    acceptors: Vec<tokio::task::JoinHandle<()>>,
}

impl Port {
    /// Stops listening; the port is free once this returns.
    async fn close(self) {
        self.stop.cancel();
        for acceptor in self.acceptors {
            if let Err(err) = acceptor.await {
                tracing::warn!(%err, "a listener task failed");
            }
        }
    }
}

struct State {
    /// Registered sessions by session domain.
    sessions: HashMap<String, Entry>,
    next_generation: u64,
    /// Set once the proxy decided to exit; no registration is accepted afterwards.
    shutting_down: bool,
    /// Since when no session has been registered.
    ///
    /// Measured on the runtime's clock, which also drives the idle tick, so a paused test
    /// runtime controls both.
    idle_since: Option<tokio::time::Instant>,
}

struct Entry {
    generation: u64,
    registration: Registration,
    /// Cancelled when the registration ends, which also ends its proxied connections.
    cancel: CancellationToken,
}

/// Why a registration was refused.
#[derive(Debug)]
struct Refusal {
    reason: String,
    transient: bool,
}

impl Refusal {
    fn transient(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            transient: true,
        }
    }
}

/// A registration owned by one socket connection.
struct Owned {
    session_domain: String,
    generation: u64,
    cancel: CancellationToken,
}

/// Where a request goes.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    pub(crate) session: String,
    pub(crate) service: Option<String>,
    pub(crate) port: u16,
    /// The matched route's path prefix, if it has one.
    pub(crate) path: Option<String>,
    /// Whether to remove `path` from the request path before forwarding.
    pub(crate) strip_prefix: bool,
    /// Cancelled when the session's registration ends.
    pub(crate) cancel: CancellationToken,
}

/// The route among `routes` that serves `path`: the one with the longest prefix of `path`, or a
/// route without a path, which matches every path, when no prefix does.
fn best_route<'a>(
    routes: impl Iterator<Item = &'a RouteTarget>,
    path: &str,
) -> Option<&'a RouteTarget> {
    routes
        .filter(|route| {
            route
                .path
                .as_deref()
                .is_none_or(|prefix| path.starts_with(prefix))
        })
        .max_by_key(|route| route.path.as_deref().map_or(0, str::len))
}

impl Shared {
    fn new(started_port: u16, stop: CancellationToken) -> Self {
        Self {
            started_port,
            state: Mutex::new(State {
                sessions: HashMap::new(),
                next_generation: 0,
                shutting_down: false,
                idle_since: Some(tokio::time::Instant::now()),
            }),
            ports: tokio::sync::Mutex::new(HashMap::new()),
            stop,
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts accepting connections on `bound`'s listeners and records them in `ports`.
    fn open(
        self: &Arc<Self>,
        ports: &mut HashMap<u16, Port>,
        bound: Bound,
    ) -> Result<(), BindError> {
        let Bound {
            port,
            listeners,
            wildcard,
        } = bound;
        let stop = self.stop.child_token();
        let mut acceptors = Vec::new();
        for listener in listeners {
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(listener) => listener,
                Err(source) => {
                    stop.cancel();
                    return Err(BindError::Io {
                        address: std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
                        source,
                    });
                }
            };
            acceptors.push(tokio::spawn(accept_http(
                listener,
                port,
                wildcard,
                self.clone(),
                stop.clone(),
            )));
        }
        ports.insert(
            port,
            Port {
                last_used: tokio::time::Instant::now(),
                stop,
                acceptors,
            },
        );
        Ok(())
    }

    /// Listens as `preference` asks, unless the proxy already listens on a port it accepts, and
    /// returns the port.
    async fn listen(self: &Arc<Self>, preference: PortPreference) -> Result<u16, Refusal> {
        let mut ports = self.ports.lock().await;
        self.listen_locked(&mut ports, preference).await
    }

    /// [`Self::listen`] with the `ports` lock already held.
    ///
    /// A failed bind is refused as final, since the session's preference cannot change.
    async fn listen_locked(
        self: &Arc<Self>,
        ports: &mut HashMap<u16, Port>,
        preference: PortPreference,
    ) -> Result<u16, Refusal> {
        if self.state().shutting_down {
            return Err(Refusal::transient("the proxy is shutting down"));
        }
        let listening = ports.keys().copied().collect::<HashSet<_>>();
        // Binding probes the port first, which can block for a moment.
        let acquired = tokio::task::spawn_blocking(move || {
            listen::acquire(preference, |port| listening.contains(&port))
        })
        .await
        .map_err(|err| Refusal::transient(format!("binding the port failed: {err}")))?
        .map_err(|err| Refusal {
            reason: err.to_string(),
            transient: false,
        })?;
        let port = match acquired {
            Acquired::Listening(port) => port,
            Acquired::Bound(bound) => {
                let port = bound.port;
                self.open(ports, bound).map_err(|err| Refusal {
                    reason: err.to_string(),
                    transient: false,
                })?;
                tracing::info!(port, "listening on another port");
                port
            }
        };
        if let Some(listener) = ports.get_mut(&port) {
            listener.last_used = tokio::time::Instant::now();
        }
        Ok(port)
    }

    /// Registers a session, listening on its port first.
    ///
    /// A port that cannot be bound is refused as retryable: the session's services already carry
    /// URLs with that port, so only the port becoming free again can help.
    async fn register_listening(
        self: &Arc<Self>,
        registration: Registration,
        previous: Option<&Owned>,
    ) -> Result<Owned, Refusal> {
        let mut ports = self.ports.lock().await;
        if registration.proxy_port != 0 {
            let exact = PortPreference {
                port: registration.proxy_port,
                fallback_port: None,
            };
            self.listen_locked(&mut ports, exact)
                .await
                .map_err(|refusal| Refusal {
                    transient: true,
                    ..refusal
                })?;
        }
        self.register(registration, previous)
    }

    /// Stops listening on every port that no session has been registered on for `idle_timeout`.
    async fn close_idle_ports(&self, idle_timeout: Duration) {
        let mut ports = self.ports.lock().await;
        let used = self
            .state()
            .sessions
            .values()
            .map(|entry| entry.registration.proxy_port)
            .collect::<HashSet<_>>();
        let now = tokio::time::Instant::now();
        let mut idle = Vec::new();
        // The port the proxy started on stays open for its whole life: `Hello` advertises it, and a
        // protocol 3 client latches the advertised port without asking the proxy to listen there.
        for (port, listener) in ports
            .iter_mut()
            .filter(|(port, _)| **port != self.started_port)
        {
            if used.contains(port) {
                listener.last_used = now;
            } else if now.duration_since(listener.last_used) >= idle_timeout {
                idle.push(*port);
            }
        }
        for port in idle {
            if let Some(listener) = ports.remove(&port) {
                listener.close().await;
                tracing::info!(port, "no session uses the port; stopped listening");
            }
        }
    }

    /// Finds the route serving `path` on `host`, among the sessions registered on `port`.
    ///
    /// The host decides first, then the path.
    /// A hostname that any route names is served by those routes alone, and every other hostname
    /// under the most specific session domain containing it by the catch-all routes.
    /// Among the chosen routes, the longest path prefix wins, and a route without a path matches
    /// every path; when none matches, nothing serves the request, rather than another host's
    /// routes answering for a mistyped path.
    pub(crate) fn lookup(&self, port: u16, host: &str, path: &str) -> Option<Target> {
        let state = self.state();
        let target = |entry: &Entry, route: &RouteTarget| Target {
            session: entry.registration.session.clone(),
            service: route.service.clone(),
            port: route.port,
            path: route.path.clone(),
            strip_prefix: route.strip_prefix,
            cancel: entry.cancel.clone(),
        };
        // Session domains never nest, so at most one contains `host`; the longest wins anyway.
        let (domain, entry) = state
            .sessions
            .iter()
            .filter(|(domain, entry)| {
                entry.registration.proxy_port == port
                    && (host == domain.as_str()
                        || host
                            .strip_suffix(domain.as_str())
                            .is_some_and(|prefix| prefix.ends_with('.')))
            })
            .max_by_key(|(domain, _)| domain.len())?;
        let labels = host
            .strip_suffix(domain.as_str())
            .and_then(|prefix| prefix.strip_suffix('.'));
        let routes = &entry.registration.routes;
        let named = |route: &&RouteTarget| {
            route.host != "*" && labels.is_some_and(|labels| route.host == labels)
        };
        let host_routes = if routes.iter().any(|route| named(&route)) {
            routes.iter().filter(named).collect::<Vec<_>>()
        } else {
            routes.iter().filter(|route| route.host == "*").collect()
        };
        best_route(host_routes.into_iter(), path).map(|route| target(entry, route))
    }

    /// The live sessions and their URLs, each with its own port, for the not-found page.
    pub(crate) fn listed_sessions(&self) -> Vec<ListedSession> {
        let state = self.state();
        let mut sessions = state
            .sessions
            .iter()
            .map(|(domain, entry)| ListedSession {
                name: entry.registration.session.clone(),
                config_path: entry.registration.config_path.clone(),
                urls: entry
                    .registration
                    .routes
                    .iter()
                    .map(|route| {
                        let suffix = match entry.registration.proxy_port {
                            80 => String::new(),
                            port => format!(":{port}"),
                        };
                        let path = route.path.as_deref().unwrap_or("/");
                        if route.host == "*" {
                            format!("http://{domain}{suffix}{path}")
                        } else {
                            format!("http://{}.{domain}{suffix}{path}", route.host)
                        }
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|a, b| a.name.cmp(&b.name));
        sessions
    }

    fn register(
        &self,
        registration: Registration,
        previous: Option<&Owned>,
    ) -> Result<Owned, Refusal> {
        let mut state = self.state();
        if state.shutting_down {
            return Err(Refusal::transient("the proxy is shutting down"));
        }
        if registration.proxy_port == 0 {
            return Err(Refusal {
                reason: "a registration must name the port its routes are served on".to_string(),
                transient: false,
            });
        }
        // Session domains are unique across ports, so a hostname means the same session whatever
        // port a request names.
        // Session domains must not nest either: `api.two` under `one.localhost` and `api` under
        // `two.one.localhost` would both claim `api.two.one.localhost`.
        let domain = registration.session_domain.as_str();
        let overlaps = |other: &str| {
            other == domain
                || domain
                    .strip_suffix(other)
                    .is_some_and(|prefix| prefix.ends_with('.'))
                || other
                    .strip_suffix(domain)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        };
        if let Some((other, existing)) = state.sessions.iter().find(|(other, existing)| {
            existing.registration.config_path != registration.config_path && overlaps(other)
        }) {
            return Err(Refusal {
                reason: format!(
                    "{domain} overlaps {other}, which the live session for {} serves; set \
                     `session.name` or `session.domain` to tell the two apart",
                    existing.registration.config_path
                ),
                transient: false,
            });
        }
        // A connection that registers its live session again only replaces the routes.
        // Keeping the generation and the cancellation keeps every proxied connection and tunnel
        // of the session open, which a takeover by another connection would end.
        if let Some(previous) = previous
            && previous.session_domain == registration.session_domain
            && let Some(entry) = state.sessions.get_mut(&previous.session_domain)
            && entry.generation == previous.generation
        {
            tracing::info!(
                session = %registration.session,
                routes = registration.routes.len(),
                generation = previous.generation,
                "session routes replaced"
            );
            entry.registration = registration;
            return Ok(Owned {
                session_domain: previous.session_domain.clone(),
                generation: previous.generation,
                cancel: previous.cancel.clone(),
            });
        }
        // A connection that switches to another session domain gives up its old one.
        if let Some(previous) = previous
            && previous.session_domain != registration.session_domain
        {
            Self::remove(&mut state, &previous.session_domain, previous.generation);
        }
        state.next_generation += 1;
        let generation = state.next_generation;
        let cancel = CancellationToken::new();
        let session_domain = registration.session_domain.clone();
        tracing::info!(
            session = %registration.session,
            routes = registration.routes.len(),
            generation,
            "session registered"
        );
        // Re-registering the same session (a reconnect) replaces the old registration, whose
        // connection then notices its cancellation and closes.
        if let Some(replaced) = state.sessions.insert(
            session_domain.clone(),
            Entry {
                generation,
                registration,
                cancel: cancel.clone(),
            },
        ) {
            replaced.cancel.cancel();
        }
        state.idle_since = None;
        Ok(Owned {
            session_domain,
            generation,
            cancel,
        })
    }

    fn unregister(&self, owned: &Owned) {
        let mut state = self.state();
        Self::remove(&mut state, &owned.session_domain, owned.generation);
    }

    /// Removes the registration of `session_domain` if it is still `generation`, so a stale
    /// connection never removes its replacement.
    fn remove(state: &mut State, session_domain: &str, generation: u64) {
        if state
            .sessions
            .get(session_domain)
            .is_some_and(|entry| entry.generation == generation)
            && let Some(entry) = state.sessions.remove(session_domain)
        {
            entry.cancel.cancel();
            tracing::info!(session = %entry.registration.session, "session unregistered");
        }
        if state.sessions.is_empty() && state.idle_since.is_none() {
            state.idle_since = Some(tokio::time::Instant::now());
        }
    }

    /// The registered sessions, sorted by name.
    fn registered_sessions(state: &State) -> Vec<RegisteredSession> {
        let mut sessions = state
            .sessions
            .values()
            .map(|entry| RegisteredSession {
                session: entry.registration.session.clone(),
                config_path: entry.registration.config_path.clone(),
            })
            .collect::<Vec<_>>();
        sessions.sort_by(|a, b| a.session.cmp(&b.session));
        sessions
    }

    /// Answers [`ClientMessage::Hello`].
    fn info(&self) -> ProxyMessage {
        ProxyMessage::Info {
            port: self.started_port,
            pid: std::process::id(),
            protocol: PROTOCOL_VERSION,
            sessions: Some(Self::registered_sessions(&self.state())),
        }
    }

    /// Decides to exit now when no session is registered, or returns the registered sessions.
    ///
    /// Like [`Self::try_begin_idle_exit`], the decision takes the registration lock, so no
    /// session registers between the check and the decision.
    fn try_stop_if_idle(&self) -> Result<(), Vec<RegisteredSession>> {
        let mut state = self.state();
        if !state.sessions.is_empty() {
            return Err(Self::registered_sessions(&state));
        }
        state.shutting_down = true;
        Ok(())
    }

    /// Ends every registration, which also ends the requests and tunnels proxied for it.
    fn end_all_registrations(&self) {
        let mut state = self.state();
        state.shutting_down = true;
        for (_, entry) in state.sessions.drain() {
            entry.cancel.cancel();
        }
    }

    /// Decides to exit when no session has been registered for `idle_timeout`.
    ///
    /// The decision and every registration take the same lock, so a registration either lands
    /// before the decision (and prevents it) or is refused after it.
    fn try_begin_idle_exit(&self, idle_timeout: Duration) -> bool {
        let mut state = self.state();
        let idle = state.sessions.is_empty()
            && state
                .idle_since
                .is_some_and(|since| since.elapsed() >= idle_timeout);
        if idle {
            state.shutting_down = true;
        }
        idle
    }
}

async fn accept_http(
    listener: tokio::net::TcpListener,
    port: u16,
    wildcard: bool,
    shared: Arc<Shared>,
    stop: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            () = stop.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(err) => {
                // Usually descriptor exhaustion; back off instead of spinning.
                tracing::warn!(%err, "failed to accept a connection");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if wildcard && !listen::is_local_peer(peer.ip()) {
            tracing::debug!(%peer, "dropping a connection from another host");
            continue;
        }
        let stop = stop.clone();
        let connection = crate::forward::serve_connection(stream, port, shared.clone());
        tokio::spawn(async move {
            tokio::select! {
                () = stop.cancelled() => {}
                () = connection => {}
            }
        });
    }
}

async fn accept_registrations(
    listener: UnixListener,
    shared: Arc<Shared>,
    stop: CancellationToken,
) {
    loop {
        let accepted = tokio::select! {
            () = stop.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, _)) => {
                tokio::spawn(serve_registration(stream, shared.clone(), stop.clone()));
            }
            Err(err) => {
                tracing::warn!(%err, "failed to accept a registration connection");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Serves one session connection; its registration ends with the connection or the proxy.
async fn serve_registration(stream: UnixStream, shared: Arc<Shared>, stop: CancellationToken) {
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(MAX_MESSAGE_BYTES));
    let mut owned: Option<Owned> = None;
    loop {
        let replaced = owned
            .as_ref()
            .map_or_else(CancellationToken::new, |owned| owned.cancel.clone());
        let line = tokio::select! {
            () = stop.cancelled() => break,
            () = replaced.cancelled() => break,
            line = tokio::time::timeout(LEASE_TIMEOUT, framed.next()) => line,
        };
        let line = match line {
            Ok(Some(Ok(line))) => line,
            Ok(Some(Err(err))) => {
                tracing::debug!(%err, "dropping a malformed registration connection");
                break;
            }
            // Closed by the session, or silent past the lease
            Ok(None) | Err(_) => break,
        };
        let reply = match serde_json::from_str::<ClientMessage>(&line) {
            Ok(ClientMessage::Hello) => shared.info(),
            Ok(ClientMessage::Listen {
                port,
                fallback_port,
            }) => {
                let preference = PortPreference {
                    port,
                    fallback_port,
                };
                match shared.listen(preference).await {
                    Ok(port) => ProxyMessage::Listening { port },
                    Err(Refusal { reason, transient }) => {
                        let rejected = ProxyMessage::Rejected { reason, transient };
                        let _ = send(&mut framed, &rejected).await;
                        break;
                    }
                }
            }
            Ok(ClientMessage::Ping) => ProxyMessage::Pong,
            Ok(ClientMessage::StopIfIdle) => match shared.try_stop_if_idle() {
                Ok(()) => {
                    tracing::info!("asked to stop while no session is registered; exiting");
                    let _ = send(&mut framed, &ProxyMessage::Stopping).await;
                    // The proxy refuses registrations from now on, so it must exit even when the
                    // reply could not be sent.
                    stop.cancel();
                    break;
                }
                Err(sessions) => ProxyMessage::Busy { sessions },
            },
            Ok(ClientMessage::Register(registration)) => {
                match shared
                    .register_listening(registration, owned.as_ref())
                    .await
                {
                    Ok(registered) => {
                        owned = Some(registered);
                        ProxyMessage::Registered
                    }
                    Err(Refusal { reason, transient }) => {
                        let rejected = ProxyMessage::Rejected { reason, transient };
                        let _ = send(&mut framed, &rejected).await;
                        break;
                    }
                }
            }
            Err(err) => {
                let rejected = ProxyMessage::Rejected {
                    reason: format!("invalid message: {err}"),
                    transient: false,
                };
                let _ = send(&mut framed, &rejected).await;
                break;
            }
        };
        if send(&mut framed, &reply).await.is_err() {
            break;
        }
    }
    if let Some(owned) = owned {
        shared.unregister(&owned);
    }
}

async fn send(
    framed: &mut Framed<UnixStream, LinesCodec>,
    message: &ProxyMessage,
) -> Result<(), tokio_util::codec::LinesCodecError> {
    let line = serde_json::to_string(message).unwrap_or_default();
    framed.send(line).await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::RouteTarget;
    use similar_asserts::assert_eq;

    pub(crate) fn registration(
        session: &str,
        config_path: &str,
        routes: &[(&str, u16)],
    ) -> Registration {
        Registration {
            session: session.to_string(),
            session_domain: format!("{session}.localhost"),
            config_path: config_path.to_string(),
            pid: 1,
            proxy_port: 80,
            routes: routes
                .iter()
                .map(|(host, port)| RouteTarget {
                    host: (*host).to_string(),
                    path: None,
                    strip_prefix: false,
                    port: *port,
                    service: None,
                })
                .collect(),
        }
    }

    fn shared() -> Shared {
        Shared::new(80, CancellationToken::new())
    }

    /// Exact routes win over the catch-all route, which serves the bare domain and every
    /// deeper name under it.
    #[test]
    fn lookup_prefers_exact_routes_then_the_catch_all() -> Result<(), Refusal> {
        let shared = shared();
        shared.register(
            registration(
                "one",
                "/one",
                &[("app", 1001), ("email.internal", 1002), ("*", 1003)],
            ),
            None,
        )?;
        shared.register(registration("two", "/two", &[("app", 2001)]), None)?;

        let port = |host: &str| shared.lookup(80, host, "/").map(|target| target.port);
        assert_eq!(port("app.one.localhost"), Some(1001));
        assert_eq!(port("email.internal.one.localhost"), Some(1002));
        assert_eq!(port("app.two.localhost"), Some(2001));
        assert_eq!(port("one.localhost"), Some(1003));
        assert_eq!(port("deep.other.one.localhost"), Some(1003));
        // A session without a catch-all serves nothing beyond its routes.
        assert_eq!(port("other.two.localhost"), None);
        assert_eq!(port("xone.localhost"), None);
        assert_eq!(port("app.three.localhost"), None);
        Ok(())
    }

    /// Within a host the longest matching path prefix wins and a route without a path serves the
    /// rest; the catch-all follows the same rule for hostnames no route names, and never answers
    /// for a named host's unmatched paths.
    #[test]
    fn lookup_picks_the_host_then_the_longest_path_prefix() -> Result<(), Refusal> {
        let shared = shared();
        let mut one = registration(
            "one",
            "/one",
            &[
                ("api", 1001),
                ("api", 1002),
                ("api", 1003),
                ("auth", 1004),
                ("*", 1005),
                ("*", 1006),
            ],
        );
        for (route, path) in one.routes.iter_mut().zip([
            None,
            Some("/rest/"),
            Some("/rest/admin/"),
            Some("/auth/v1/"),
            None,
            Some("/static/"),
        ]) {
            route.path = path.map(ToString::to_string);
        }
        shared.register(one, None)?;

        let port = |host: &str, path: &str| shared.lookup(80, host, path).map(|target| target.port);

        // Longest prefix within the host, then the host's route without a path
        assert_eq!(port("api.one.localhost", "/rest/admin/users"), Some(1003));
        assert_eq!(port("api.one.localhost", "/rest/x"), Some(1002));
        assert_eq!(port("api.one.localhost", "/rest/"), Some(1002));
        assert_eq!(port("api.one.localhost", "/rest"), Some(1001));
        assert_eq!(port("api.one.localhost", "/REST/x"), Some(1001));
        assert_eq!(port("api.one.localhost", "/static/x"), Some(1001));

        // A named host whose path routes do not match is not handed to the catch-all.
        assert_eq!(port("auth.one.localhost", "/auth/v1/token"), Some(1004));
        assert_eq!(port("auth.one.localhost", "/other"), None);
        assert_eq!(port("auth.one.localhost", "/static/x"), None);

        // The catch-all's own path routes and its fallback serve every unnamed hostname.
        assert_eq!(port("one.localhost", "/static/app.js"), Some(1006));
        assert_eq!(port("deep.other.one.localhost", "/"), Some(1005));
        assert_eq!(port("web.one.localhost", "/static/x"), Some(1006));
        Ok(())
    }

    /// Without a fallback anywhere, a path that no prefix matches is not served.
    #[test]
    fn unmatched_paths_without_a_fallback_are_not_found() -> Result<(), Refusal> {
        let shared = shared();
        let mut one = registration("one", "/one", &[("auth", 1001)]);
        if let Some(route) = one.routes.first_mut() {
            route.path = Some("/auth/v1/".to_string());
            route.strip_prefix = true;
        }
        shared.register(one, None)?;

        let target = shared.lookup(80, "auth.one.localhost", "/auth/v1/token");
        assert_eq!(
            target.map(|target| (target.port, target.path, target.strip_prefix)),
            Some((1001, Some("/auth/v1/".to_string()), true))
        );
        assert!(
            shared
                .lookup(80, "auth.one.localhost", "/auth/v1")
                .is_none()
        );
        assert!(shared.lookup(80, "auth.one.localhost", "/").is_none());
        Ok(())
    }

    /// A second config cannot take over a live session's domain, while the same config (a
    /// reconnect) replaces its own registration and the stale connection cannot remove it.
    #[test]
    fn registrations_are_owned_by_config_and_generation() -> Result<(), Refusal> {
        let shared = shared();
        let first = shared.register(registration("one", "/one", &[("app", 1001)]), None)?;

        let conflict = shared.register(registration("one", "/elsewhere", &[("app", 9)]), None);
        assert!(conflict.is_err());

        let second = shared.register(registration("one", "/one", &[("app", 1002)]), None)?;
        assert!(first.cancel.is_cancelled());
        shared.unregister(&first);
        assert_eq!(
            shared.lookup(80, "app.one.localhost", "/").map(|t| t.port),
            Some(1002)
        );

        shared.unregister(&second);
        assert!(shared.lookup(80, "app.one.localhost", "/").is_none());
        assert!(second.cancel.is_cancelled());
        Ok(())
    }

    /// A connection that registers its live session again swaps the routes in place, so the
    /// session's proxied connections stay open; a reconnect from another connection still takes
    /// the session over and ends them.
    #[test]
    fn re_registering_on_the_same_connection_keeps_proxied_connections() -> Result<(), Refusal> {
        let shared = shared();
        let first = shared.register(registration("one", "/one", &[("app", 1001)]), None)?;
        let target = shared
            .lookup(80, "app.one.localhost", "/")
            .ok_or_else(|| Refusal::transient("app is not routed"))?;

        // Same connection
        let replaced = shared.register(
            registration("one", "/one", &[("app", 1002), ("api", 1003)]),
            Some(&first),
        )?;
        assert_eq!(replaced.generation, first.generation);
        assert!(!target.cancel.is_cancelled());
        assert_eq!(
            shared.lookup(80, "app.one.localhost", "/").map(|t| t.port),
            Some(1002)
        );
        assert_eq!(
            shared.lookup(80, "api.one.localhost", "/").map(|t| t.port),
            Some(1003)
        );

        // Another connection
        let takeover = shared.register(registration("one", "/one", &[("app", 1004)]), None)?;
        assert!(target.cancel.is_cancelled());
        shared.unregister(&replaced);
        assert_eq!(
            shared.lookup(80, "app.one.localhost", "/").map(|t| t.port),
            Some(1004)
        );
        shared.unregister(&takeover);
        Ok(())
    }

    /// Session domains that nest inside each other could claim the same hostname, so the second
    /// is refused, even on another port; a registration that names no port is refused too.
    #[test]
    fn overlapping_domains_and_portless_registrations_are_refused() -> Result<(), Refusal> {
        let shared = shared();
        let mut outer = registration("one", "/one", &[("api.two", 1)]);
        outer.session_domain = "one.localhost".to_string();
        shared.register(outer, None)?;

        let mut inner = registration("two", "/two", &[("api", 2)]);
        inner.session_domain = "two.one.localhost".to_string();
        inner.proxy_port = 8080;
        let refused = shared.register(inner, None);
        assert!(matches!(
            refused,
            Err(Refusal {
                transient: false,
                ..
            })
        ));

        let mut portless = registration("three", "/three", &[("app", 3)]);
        portless.proxy_port = 0;
        let refused = shared.register(portless, None);
        assert!(matches!(
            refused,
            Err(Refusal {
                transient: false,
                ..
            })
        ));
        Ok(())
    }

    /// A request reaches only the sessions registered on the port it arrived on, and the
    /// not-found page lists every session with its own port.
    #[test]
    fn lookup_serves_each_session_on_its_own_port() -> Result<(), Refusal> {
        let shared = shared();
        shared.register(registration("one", "/one", &[("app", 1001)]), None)?;
        let mut two = registration("two", "/two", &[("app", 2001)]);
        two.proxy_port = 8080;
        shared.register(two, None)?;

        let port = |proxy_port: u16, host: &str| {
            shared
                .lookup(proxy_port, host, "/")
                .map(|target| target.port)
        };
        assert_eq!(port(80, "app.one.localhost"), Some(1001));
        assert_eq!(port(8080, "app.two.localhost"), Some(2001));
        assert_eq!(port(8080, "app.one.localhost"), None);
        assert_eq!(port(80, "app.two.localhost"), None);

        let urls = shared
            .listed_sessions()
            .into_iter()
            .flat_map(|session| session.urls)
            .collect::<Vec<_>>();
        assert_eq!(
            urls,
            [
                "http://app.one.localhost/",
                "http://app.two.localhost:8080/"
            ]
        );
        Ok(())
    }

    /// Once the proxy decided to exit, registrations are refused, so none can be lost with it.
    #[test]
    fn idle_exit_refuses_later_registrations() {
        let shared = shared();
        assert!(shared.try_begin_idle_exit(Duration::ZERO));
        assert!(
            shared
                .register(registration("one", "/one", &[("app", 1)]), None)
                .is_err()
        );
    }

    /// A registered session keeps the proxy alive.
    #[test]
    fn registered_sessions_prevent_idle_exit() -> Result<(), Refusal> {
        let shared = shared();
        let owned = shared.register(registration("one", "/one", &[("app", 1)]), None)?;
        assert!(!shared.try_begin_idle_exit(Duration::ZERO));
        shared.unregister(&owned);
        assert!(shared.try_begin_idle_exit(Duration::ZERO));
        Ok(())
    }

    /// A request to stop is refused with the registered sessions while any is registered, and
    /// otherwise decides the exit at once, so a session registering afterwards is refused rather
    /// than lost with the proxy.
    #[test]
    fn stop_if_idle_stops_only_without_sessions() -> Result<(), Refusal> {
        let shared = shared();
        let owned = shared.register(registration("one", "/one", &[("app", 1)]), None)?;
        assert_eq!(
            shared.try_stop_if_idle(),
            Err(vec![RegisteredSession {
                session: "one".to_string(),
                config_path: "/one".to_string(),
            }])
        );
        shared.unregister(&owned);
        assert_eq!(shared.try_stop_if_idle(), Ok(()));
        let refused = shared
            .register(registration("two", "/two", &[("app", 2)]), None)
            .err()
            .map(|refusal| refusal.transient);
        assert_eq!(refused, Some(true));
        Ok(())
    }
}
