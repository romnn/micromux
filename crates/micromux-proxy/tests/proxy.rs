//! End-to-end tests of the proxy: a real server, real registrations, and raw TCP clients.
#![cfg(unix)]

use std::time::Duration;

use color_eyre::eyre;
use micromux_proxy::protocol::{Registration, RouteTarget};
use micromux_proxy::{PortPreference, ProxyPaths, Registrar, ServeOptions, ensure_proxy, serve};
use similar_asserts::assert_eq;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

/// A proxy running in this process on an ephemeral port.
struct Proxy {
    paths: ProxyPaths,
    port: u16,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<Result<(), micromux_proxy::ServeError>>,
    _directory: tempfile::TempDir,
}

async fn start_proxy(idle_timeout: Duration) -> eyre::Result<Proxy> {
    let directory = tempfile::tempdir()?;
    let paths = ProxyPaths::in_dir(directory.path());
    let shutdown = CancellationToken::new();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(serve(
        ServeOptions {
            paths: paths.clone(),
            preference: PortPreference {
                port: 0,
                fallback_port: None,
            },
            idle_timeout,
        },
        shutdown.clone(),
        move |port| {
            let _ = ready_tx.send(port);
        },
    ));
    let port = tokio::time::timeout(Duration::from_secs(5), ready_rx).await??;
    Ok(Proxy {
        paths,
        port,
        shutdown,
        task,
        _directory: directory,
    })
}

fn registration(session: &str, routes: &[(&str, u16, Option<&str>)]) -> Registration {
    Registration {
        proxy_port: 0,
        session: session.to_string(),
        session_domain: format!("{session}.localhost"),
        config_path: format!("/projects/{session}/micromux.yaml"),
        pid: std::process::id(),
        routes: routes
            .iter()
            .map(|(host, port, service)| RouteTarget {
                host: (*host).to_string(),
                path: None,
                strip_prefix: false,
                port: *port,
                service: service.map(ToString::to_string),
            })
            .collect(),
    }
}

/// Reserves a loopback port that refuses connections, for routes whose upstream must be down.
///
/// The socket is bound but never listens, so connecting to it is refused while no other
/// process, such as a test running in parallel, can listen on the port until the socket drops.
/// A port merely freed after use could be handed to such a process right away.
fn refusing_port() -> eyre::Result<(tokio::net::TcpSocket, u16)> {
    let socket = tokio::net::TcpSocket::new_v4()?;
    socket.bind((std::net::Ipv4Addr::LOCALHOST, 0).into())?;
    let port = socket.local_addr()?.port();
    Ok((socket, port))
}

/// A route of `host` that serves only paths under `path`, removing `path` first when `strip`.
fn path_route(host: &str, path: &str, strip: bool, port: u16) -> RouteTarget {
    RouteTarget {
        host: host.to_string(),
        path: Some(path.to_string()),
        strip_prefix: strip,
        port,
        service: None,
    }
}

/// Answers one request on `upstream` with `body` and returns the request head it received.
fn answer_once(
    upstream: TcpListener,
    body: &'static str,
) -> tokio::task::JoinHandle<eyre::Result<String>> {
    tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await?;
        let head = read_head(&mut stream).await?;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await?;
        Ok(head)
    })
}

/// Sends a `Connection: close` GET for `target` on `host` through the proxy and returns the whole
/// response.
async fn get_target(proxy_port: u16, host: &str, target: &str) -> eyre::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port)).await?;
    stream
        .write_all(
            format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    let mut response = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        stream.read_to_string(&mut response),
    )
    .await??;
    Ok(response)
}

/// Reads one request head from `stream`.
async fn read_head(stream: &mut TcpStream) -> eyre::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await? == 0 {
            eyre::bail!("connection closed before the head ended");
        }
        head.push(byte[0]);
    }
    Ok(String::from_utf8(head)?)
}

/// Sends a `Connection: close` GET through the proxy and returns the whole response.
async fn get(proxy_port: u16, host: &str) -> eyre::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", proxy_port)).await?;
    stream
        .write_all(
            format!("GET /path?q=1 HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    let mut response = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        stream.read_to_string(&mut response),
    )
    .await??;
    Ok(response)
}

/// Requests reach the registered upstream with their `Host` header and path intact, and the
/// response streams back.
#[tokio::test]
async fn routes_by_host_and_preserves_the_host_header() -> eyre::Result<()> {
    let proxy = start_proxy(Duration::from_secs(60)).await?;
    let upstream = TcpListener::bind(("127.0.0.1", 0)).await?;
    let upstream_port = upstream.local_addr()?.port();
    let _registrar = Registrar::start(
        proxy.paths.clone(),
        registration("one", &[("app", upstream_port, Some("web"))]),
        proxy.port,
        None,
        &CancellationToken::new(),
        |_| {},
    )
    .await?;

    let server = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await?;
        let head = read_head(&mut stream).await?;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello")
            .await?;
        Ok::<_, eyre::Report>(head)
    });
    let response = get(proxy.port, &format!("App.One.localhost:{}", proxy.port)).await?;
    let head = server.await??;

    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.ends_with("hello"), "{response}");
    assert!(head.starts_with("GET /path?q=1 HTTP/1.1\r\n"), "{head}");
    // The upstream sees the browser's Host, port included, not its own address.
    assert!(
        head.to_ascii_lowercase()
            .contains(&format!("host: app.one.localhost:{}\r\n", proxy.port)),
        "{head}"
    );

    proxy.shutdown.cancel();
    proxy.task.await??;
    Ok(())
}

/// Path routes split one host between upstreams by the longest matching prefix, a stripped
/// prefix never reaches its upstream, and the path, query, and `Host` header otherwise pass
/// through unchanged.
#[tokio::test]
async fn path_routes_split_a_host_and_strip_prefixes() -> eyre::Result<()> {
    let proxy = start_proxy(Duration::from_secs(60)).await?;
    let rest = TcpListener::bind(("127.0.0.1", 0)).await?;
    let grpc = TcpListener::bind(("127.0.0.1", 0)).await?;
    let auth = TcpListener::bind(("127.0.0.1", 0)).await?;
    let mut one = registration("one", &[("api", grpc.local_addr()?.port(), None)]);
    one.routes.extend([
        path_route("api", "/rest/", false, rest.local_addr()?.port()),
        path_route("auth", "/auth/v1/", true, auth.local_addr()?.port()),
    ]);
    let _registrar = Registrar::start(
        proxy.paths.clone(),
        one,
        proxy.port,
        None,
        &CancellationToken::new(),
        |_| {},
    )
    .await?;

    // Prefix route, kept prefix
    let server = answer_once(rest, "rest");
    let response = get_target(proxy.port, "api.one.localhost", "/rest/users?page=2").await?;
    let head = server.await??;
    assert!(response.ends_with("rest"), "{response}");
    assert!(
        head.starts_with("GET /rest/users?page=2 HTTP/1.1\r\n"),
        "{head}"
    );

    // The host's route without a path serves every other path, including the bare prefix.
    let server = answer_once(grpc, "grpc");
    let response = get_target(proxy.port, "api.one.localhost", "/rest").await?;
    let head = server.await??;
    assert!(response.ends_with("grpc"), "{response}");
    assert!(head.starts_with("GET /rest HTTP/1.1\r\n"), "{head}");

    // Stripped prefix, with the browser's Host intact
    let server = answer_once(auth, "auth");
    let response = get_target(proxy.port, "auth.one.localhost", "/auth/v1/token?grant=pw").await?;
    let head = server.await??;
    assert!(response.ends_with("auth"), "{response}");
    assert!(
        head.starts_with("GET /token?grant=pw HTTP/1.1\r\n"),
        "{head}"
    );
    assert!(
        head.to_ascii_lowercase()
            .contains("host: auth.one.localhost\r\n"),
        "{head}"
    );

    // A path no route of the host serves, without a catch-all, lists the routes with their paths.
    let missing = get_target(proxy.port, "auth.one.localhost", "/other").await?;
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    assert!(missing.contains("auth.one.localhost/other"), "{missing}");
    assert!(
        missing.contains(&format!(
            "http://auth.one.localhost:{}/auth/v1/",
            proxy.port
        )),
        "{missing}"
    );

    proxy.shutdown.cancel();
    proxy.task.await??;
    Ok(())
}

/// An upgrade through a stripped path route reaches the upstream at the stripped path and then
/// tunnels bytes both ways.
#[tokio::test]
async fn upgrades_tunnel_through_stripped_path_routes() -> eyre::Result<()> {
    let proxy = start_proxy(Duration::from_secs(60)).await?;
    let upstream = TcpListener::bind(("127.0.0.1", 0)).await?;
    let mut one = registration("one", &[]);
    one.routes.push(path_route(
        "realtime",
        "/realtime/v1",
        true,
        upstream.local_addr()?.port(),
    ));
    let _registrar = Registrar::start(
        proxy.paths.clone(),
        one,
        proxy.port,
        None,
        &CancellationToken::new(),
        |_| {},
    )
    .await?;

    // An echo server behind an upgrade handshake, which reports the head it was asked with
    let (head_tx, head_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await?;
        let _ = head_tx.send(read_head(&mut stream).await?);
        stream
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
            )
            .await?;
        let mut buffer = [0u8; 64];
        loop {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                return Ok::<_, eyre::Report>(());
            }
            stream
                .write_all(buffer.get(..read).unwrap_or_default())
                .await?;
        }
    });

    let mut client = TcpStream::connect(("127.0.0.1", proxy.port)).await?;
    client
        .write_all(b"GET /realtime/v1/websocket?vsn=1.0.0 HTTP/1.1\r\nHost: realtime.one.localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
        .await?;
    let response = read_head(&mut client).await?;
    assert!(response.starts_with("HTTP/1.1 101"), "{response}");
    let head = head_rx.await?;
    assert!(
        head.starts_with("GET /websocket?vsn=1.0.0 HTTP/1.1\r\n"),
        "{head}"
    );
    assert!(
        head.to_ascii_lowercase().contains("upgrade: websocket"),
        "{head}"
    );

    // The tunnel carries bytes both ways after the switch.
    client.write_all(b"ping").await?;
    let mut echoed = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut echoed)).await??;
    assert_eq!(&echoed, b"ping");

    drop(client);
    server.await??;
    proxy.shutdown.cancel();
    proxy.task.await??;
    Ok(())
}

/// A stand-in for a proxy started by an older micromux, listening on `paths`.
///
/// It answers `hello` with `info` and `stop_if_idle` with `stop_reply` (an older proxy rejects
/// the request it does not know), accepts every registration, and reports every message it
/// receives on the returned channel.
/// It never exits by itself; aborting the task stops accepting connections.
fn spawn_old_proxy(
    paths: &ProxyPaths,
    info: String,
    stop_reply: String,
) -> eyre::Result<(
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::UnboundedReceiver<micromux_proxy::protocol::ClientMessage>,
)> {
    use micromux_proxy::protocol::ClientMessage;

    let listener = tokio::net::UnixListener::bind(&paths.socket)?;
    let (received_tx, received) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let info = info.clone();
            let stop_reply = stop_reply.clone();
            let received_tx = received_tx.clone();
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut lines = tokio::io::BufReader::new(reader);
                let mut line = String::new();
                while tokio::io::AsyncBufReadExt::read_line(&mut lines, &mut line)
                    .await
                    .is_ok_and(|read| read > 0)
                {
                    let message = serde_json::from_str::<ClientMessage>(&line);
                    let reply = match &message {
                        Ok(ClientMessage::Hello) => info.as_str(),
                        Ok(ClientMessage::Register(_)) => r#"{"type":"registered"}"#,
                        Ok(ClientMessage::StopIfIdle) => stop_reply.as_str(),
                        _ => r#"{"type":"pong"}"#,
                    };
                    if let Ok(message) = message {
                        let _ = received_tx.send(message);
                    }
                    line.clear();
                    if writer
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    Ok((task, received))
}

/// What an older proxy that speaks protocol 1 replies to `stop_if_idle`, which it does not know.
const UNKNOWN_REQUEST: &str =
    r#"{"type":"rejected","reason":"invalid message: unknown variant","transient":false}"#;

/// A process whose pid an outdated stand-in proxy reports as its own, so a test can tell that
/// nobody signalled it.
struct Bystander(std::process::Child);

impl Bystander {
    fn spawn() -> eyre::Result<Self> {
        Ok(Self(
            std::process::Command::new("sleep")
                .arg("600")
                .stdin(std::process::Stdio::null())
                .spawn()?,
        ))
    }

    fn pid(&self) -> u32 {
        self.0.id()
    }

    /// Whether the process still runs.
    fn alive(&mut self) -> eyre::Result<bool> {
        Ok(self.0.try_wait()?.is_none())
    }
}

impl Drop for Bystander {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Takes over the registration socket of `old`'s directory for `proxy`, the way a fresh proxy
/// replaces an exited one: renaming the socket replaces the stand-in's in one step.
fn replace_old_proxy(old: &ProxyPaths, proxy: &Proxy) -> eyre::Result<()> {
    std::fs::rename(&proxy.paths.socket, &old.socket)?;
    Ok(())
}

/// A proxy that does not report the path-route protocol, as one started by an older micromux,
/// would ignore route paths, so a session with path routes does not register with it.
/// Such a proxy cannot tell whether sessions use it, so the session waits for it to exit, bounded
/// by its idle timeout plus a lease, and then refuses it with an error naming the proxy, while a
/// session without paths still registers until its routes gain a path.
/// The session never signals the pid the proxy reports.
#[tokio::test(start_paused = true)]
async fn path_routes_wait_for_an_outdated_proxy_and_refuse_one_that_stays() -> eyre::Result<()> {
    use micromux_proxy::protocol::{IDLE_TIMEOUT, LEASE_TIMEOUT, ProxyMessage};

    let directory = tempfile::tempdir()?;
    let paths = ProxyPaths::in_dir(directory.path());
    let mut bystander = Bystander::spawn()?;
    let pid = bystander.pid();
    let info = format!(r#"{{"type":"info","port":1,"pid":{pid}}}"#);
    // The stand-in's reply is what an older proxy sends: no protocol, read as the first one, and
    // no sessions, which it cannot tell.
    assert!(matches!(
        serde_json::from_str::<ProxyMessage>(&info)?,
        ProxyMessage::Info {
            protocol: 1,
            sessions: None,
            ..
        }
    ));
    let (old_proxy, mut received) = spawn_old_proxy(&paths, info, UNKNOWN_REQUEST.to_string())?;

    // The session waits, says so, and gives up once the bound has passed.
    let mut with_paths = registration("one", &[]);
    with_paths
        .routes
        .push(path_route("api", "/rest/", false, 2));
    let started = tokio::time::Instant::now();
    let mut waits = Vec::new();
    let refused = Registrar::start(
        paths.clone(),
        with_paths,
        1,
        None,
        &CancellationToken::new(),
        |waiting| waits.push(waiting.to_string()),
    )
    .await;
    let waited = started.elapsed();
    assert!(
        matches!(
            &refused,
            Err(micromux_proxy::ClientError::OutdatedProxy {
                pid: refused_pid,
                protocol: 1,
                required: 2,
                sessions: None,
            }) if *refused_pid == pid
        ),
        "{refused:?}"
    );
    assert_eq!(
        waits,
        [format!(
            "waiting for the outdated proxy (pid {pid}) to exit"
        )]
    );
    // The paused clock jumps to the next timer while a request waits for its reply, which can be
    // its reply timeout, so the upper end of the wait is loose.
    let bound = IDLE_TIMEOUT + LEASE_TIMEOUT;
    assert!(waited >= bound && waited < bound * 2, "{waited:?}");
    // A proxy that cannot tell is never asked to stop, and its pid is never signalled.
    while let Ok(message) = received.try_recv() {
        assert!(
            !matches!(message, micromux_proxy::protocol::ClientMessage::StopIfIdle),
            "{message:?}"
        );
    }
    assert!(bystander.alive()?);

    let without_paths = Registrar::start(
        paths,
        registration("two", &[("app", 3, None)]),
        1,
        None,
        &CancellationToken::new(),
        |_| {},
    )
    .await?;

    // Routes that gain a path later are not sent over the connection checked without one; the
    // reconnect waits for the old proxy, then refuses it and reports why.
    let mut status = without_paths.status();
    without_paths
        .routes()
        .send_replace(vec![path_route("app", "/rest/", false, 3)]);
    // No timeout guards these waits: on the paused clock, a timer that fires while the
    // registrar waits for a reply would jump the clock past the whole wait.
    let waiting = format!("waiting for the outdated proxy (pid {pid}) to exit");
    status
        .wait_for(|status| {
            matches!(
                status,
                micromux_proxy::RegistrationStatus::Unavailable { reason } if *reason == waiting
            )
        })
        .await?;
    status
        .wait_for(|status| {
            matches!(
                status,
                micromux_proxy::RegistrationStatus::Unavailable { reason }
                    if reason.contains("registration protocol 1")
            )
        })
        .await?;
    assert!(bystander.alive()?);

    old_proxy.abort();
    Ok(())
}

/// An outdated proxy that cannot tell whether sessions use it, and exits while the session
/// waits, is replaced by a current proxy that takes the session's path routes.
#[tokio::test]
async fn path_routes_take_over_an_outdated_proxy_once_it_exits() -> eyre::Result<()> {
    let proxy = start_proxy(Duration::from_secs(60)).await?;
    let directory = tempfile::tempdir()?;
    let paths = ProxyPaths::in_dir(directory.path());
    let mut bystander = Bystander::spawn()?;
    let pid = bystander.pid();
    let (old_proxy, _received) = spawn_old_proxy(
        &paths,
        format!(r#"{{"type":"info","port":{},"pid":{pid}}}"#, proxy.port),
        UNKNOWN_REQUEST.to_string(),
    )?;
    let upstream = TcpListener::bind(("127.0.0.1", 0)).await?;
    let mut one = registration("one", &[]);
    one.routes.push(path_route(
        "api",
        "/rest/",
        true,
        upstream.local_addr()?.port(),
    ));

    // The old proxy exits once the session announced its wait, and a current one takes its
    // place.
    let (waiting_tx, mut waiting_rx) = tokio::sync::mpsc::unbounded_channel();
    let exit = async {
        let waiting = waiting_rx.recv().await;
        old_proxy.abort();
        replace_old_proxy(&paths, &proxy)?;
        Ok::<_, eyre::Report>(waiting)
    };
    let shutdown = CancellationToken::new();
    let (registrar, waiting) = tokio::join!(
        Registrar::start(
            paths.clone(),
            one,
            proxy.port,
            None,
            &shutdown,
            move |waiting| {
                let _ = waiting_tx.send(waiting.to_string());
            },
        ),
        exit
    );
    let _registrar = registrar?;
    assert_eq!(
        waiting?,
        Some(format!(
            "waiting for the outdated proxy (pid {pid}) to exit"
        ))
    );

    // The current proxy serves the path route.
    let served = answer_once(upstream, "rest");
    let response = get_target(proxy.port, "api.one.localhost", "/rest/x").await?;
    assert!(response.ends_with("rest"), "{response}");
    assert!(served.await??.starts_with("GET /x HTTP/1.1\r\n"));
    assert!(bystander.alive()?);

    proxy.shutdown.cancel();
    proxy.task.await??;
    Ok(())
}

/// An outdated proxy that lists no registered session is asked to stop, and replaced at once.
///
/// Every proxy from protocol 3 on lists its sessions, so this is how a session gets past a
/// protocol 3 proxy once its routes need a later protocol.
#[tokio::test]
async fn an_idle_outdated_proxy_that_lists_its_sessions_is_stopped() -> eyre::Result<()> {
    use micromux_proxy::protocol::ClientMessage;

    let proxy = start_proxy(Duration::from_secs(60)).await?;
    let directory = tempfile::tempdir()?;
    let paths = ProxyPaths::in_dir(directory.path());
    let (old_proxy, mut received) = spawn_old_proxy(
        &paths,
        format!(
            r#"{{"type":"info","port":{},"pid":4242,"protocol":1,"sessions":[]}}"#,
            proxy.port
        ),
        r#"{"type":"stopping"}"#.to_string(),
    )?;

    // The stand-in exits once asked to stop, and a current proxy takes its place.
    let exit = async {
        loop {
            match received.recv().await {
                Some(ClientMessage::StopIfIdle) => break,
                Some(_) => {}
                None => eyre::bail!("the stand-in stopped before it was asked to"),
            }
        }
        old_proxy.abort();
        replace_old_proxy(&paths, &proxy)?;
        Ok(())
    };
    let mut one = registration("one", &[]);
    one.routes.push(path_route("api", "/rest/", false, 2));
    let shutdown = CancellationToken::new();
    let (registrar, exited) = tokio::join!(
        Registrar::start(paths.clone(), one, proxy.port, None, &shutdown, |_| {},),
        exit
    );
    exited?;
    let registrar = registrar?;
    assert_eq!(
        *registrar.status().borrow(),
        micromux_proxy::RegistrationStatus::Registered
    );

    proxy.shutdown.cancel();
    proxy.task.await??;
    Ok(())
}

/// An outdated proxy that still serves other sessions is refused at once with their names,
/// whether its `hello` lists them or a session registered just before the request to stop.
#[tokio::test]
async fn a_busy_outdated_proxy_is_refused_with_its_sessions() -> eyre::Result<()> {
    use micromux_proxy::protocol::ClientMessage;

    let busy = r#"[{"session":"old","config_path":"/old/micromux.yaml"}]"#;
    for (info_sessions, stop_reply) in [
        (busy.to_string(), UNKNOWN_REQUEST.to_string()),
        (
            "[]".to_string(),
            format!(r#"{{"type":"busy","sessions":{busy}}}"#),
        ),
    ] {
        let directory = tempfile::tempdir()?;
        let paths = ProxyPaths::in_dir(directory.path());
        let (old_proxy, mut received) = spawn_old_proxy(
            &paths,
            format!(
                r#"{{"type":"info","port":1,"pid":4242,"protocol":1,"sessions":{info_sessions}}}"#
            ),
            stop_reply,
        )?;
        let mut one = registration("one", &[]);
        one.routes.push(path_route("api", "/rest/", false, 2));
        let mut waits = Vec::new();
        let refused = Registrar::start(paths, one, 1, None, &CancellationToken::new(), |waiting| {
            waits.push(waiting.to_string());
        })
        .await;

        // The error names the old proxy and the session it serves, without any wait.
        let Err(err @ micromux_proxy::ClientError::OutdatedProxy { sessions, .. }) = &refused
        else {
            eyre::bail!("{refused:?}");
        };
        assert_eq!(sessions.as_ref().map(Vec::len), Some(1));
        let message = err.to_string();
        assert!(message.contains("pid 4242"), "{message}");
        assert!(message.contains("old (/old/micromux.yaml)"), "{message}");
        assert!(waits.is_empty(), "{waits:?}");
        // A proxy that lists sessions is not even asked to stop.
        if info_sessions == busy {
            while let Ok(message) = received.try_recv() {
                assert!(!matches!(message, ClientMessage::StopIfIdle), "{message:?}");
            }
        }
        old_proxy.abort();
    }
    Ok(())
}

/// An outdated proxy that does not list its sessions over the socket still lists them on its
/// not-found page, so a session refuses it at once, naming the sessions that use it, and stops
/// waiting for an idle one as soon as a session registers with it.
#[tokio::test]
async fn an_outdated_proxy_is_refused_with_the_sessions_its_page_lists() -> eyre::Result<()> {
    // The stand-in speaks protocol 1 over the socket, and a current proxy serves the HTTP port
    // it reports, whose not-found page is the one every older proxy serves too.
    let proxy = start_proxy(Duration::from_secs(60)).await?;
    let directory = tempfile::tempdir()?;
    let paths = ProxyPaths::in_dir(directory.path());
    let (old_proxy, _received) = spawn_old_proxy(
        &paths,
        format!(r#"{{"type":"info","port":{},"pid":4242}}"#, proxy.port),
        UNKNOWN_REQUEST.to_string(),
    )?;
    let with_paths = || {
        let mut one = registration("one", &[]);
        one.routes.push(path_route("api", "/rest/", false, 2));
        one
    };
    let old = |session: &str| micromux_proxy::protocol::RegisteredSession {
        session: session.to_string(),
        config_path: format!("/projects/{session}/micromux.yaml"),
    };

    // Idle: the session waits, and a session registering meanwhile ends the wait with its name.
    let shutdown = CancellationToken::new();
    let (waiting_tx, mut waiting_rx) = tokio::sync::mpsc::unbounded_channel();
    let register_old = async {
        waiting_rx.recv().await;
        Registrar::start(
            proxy.paths.clone(),
            registration("two", &[("app", 1, None)]),
            proxy.port,
            None,
            &shutdown,
            |_| {},
        )
        .await
    };
    let (refused, two) = tokio::join!(
        Registrar::start(
            paths.clone(),
            with_paths(),
            proxy.port,
            None,
            &shutdown,
            move |waiting| {
                let _ = waiting_tx.send(waiting.to_string());
            },
        ),
        register_old
    );
    let _two = two?;
    assert!(
        matches!(
            &refused,
            Err(micromux_proxy::ClientError::OutdatedProxy {
                pid: 4242,
                sessions: Some(sessions),
                ..
            }) if *sessions == [old("two")]
        ),
        "{refused:?}"
    );

    // Busy: the session refuses it right away.
    let mut waits = Vec::new();
    let refused = Registrar::start(
        paths,
        with_paths(),
        proxy.port,
        None,
        &shutdown,
        |waiting| {
            waits.push(waiting.to_string());
        },
    )
    .await;
    let Err(err @ micromux_proxy::ClientError::OutdatedProxy { .. }) = &refused else {
        eyre::bail!("{refused:?}");
    };
    assert!(
        err.to_string()
            .contains("two (/projects/two/micromux.yaml)"),
        "{err}"
    );
    assert!(waits.is_empty(), "{waits:?}");

    old_proxy.abort();
    proxy.shutdown.cancel();
    proxy.task.await??;
    Ok(())
}

/// A session that is asked to stop while it waits for an outdated proxy stops waiting.
#[tokio::test]
async fn waiting_for_an_outdated_proxy_ends_with_the_session() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let paths = ProxyPaths::in_dir(directory.path());
    let (old_proxy, _received) = spawn_old_proxy(
        &paths,
        r#"{"type":"info","port":1,"pid":4242}"#.to_string(),
        UNKNOWN_REQUEST.to_string(),
    )?;
    let shutdown = CancellationToken::new();
    let mut one = registration("one", &[]);
    one.routes.push(path_route("api", "/rest/", false, 2));
    let stop = shutdown.clone();
    let cancelled = tokio::time::timeout(
        Duration::from_secs(10),
        Registrar::start(paths, one, 1, None, &shutdown, move |_| stop.cancel()),
    )
    .await?;
    assert!(
        matches!(cancelled, Err(micromux_proxy::ClientError::Cancelled)),
        "{cancelled:?}"
    );
    old_proxy.abort();
    Ok(())
}

/// Sends `message` on a fresh connection and returns the reply.
async fn ask(
    paths: &ProxyPaths,
    message: &micromux_proxy::protocol::ClientMessage,
) -> eyre::Result<micromux_proxy::protocol::ProxyMessage> {
    let stream = tokio::net::UnixStream::connect(&paths.socket).await?;
    let (reader, mut writer) = stream.into_split();
    writer
        .write_all(format!("{}\n", serde_json::to_string(message)?).as_bytes())
        .await?;
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        tokio::io::AsyncBufReadExt::read_line(&mut tokio::io::BufReader::new(reader), &mut line),
    )
    .await??;
    Ok(serde_json::from_str(&line)?)
}

/// A current proxy lists its registered sessions, refuses to stop while any is registered, and
/// exits at once when asked to stop without one.
#[tokio::test]
async fn the_proxy_lists_its_sessions_and_stops_only_when_idle() -> eyre::Result<()> {
    use micromux_proxy::protocol::{
        ClientMessage, PROTOCOL_VERSION, ProxyMessage, RegisteredSession,
    };

    let proxy = start_proxy(Duration::from_secs(60)).await?;

    let registrar = Registrar::start(
        proxy.paths.clone(),
        registration("one", &[("app", 1, None)]),
        proxy.port,
        None,
        &CancellationToken::new(),
        |_| {},
    )
    .await?;
    let one = RegisteredSession {
        session: "one".to_string(),
        config_path: "/projects/one/micromux.yaml".to_string(),
    };

    // A registered session is listed, and keeps the proxy from stopping.
    assert_eq!(
        ask(&proxy.paths, &ClientMessage::Hello).await?,
        ProxyMessage::Info {
            port: proxy.port,
            pid: std::process::id(),
            protocol: PROTOCOL_VERSION,
            sessions: Some(vec![one.clone()]),
        }
    );
    assert_eq!(
        ask(&proxy.paths, &ClientMessage::StopIfIdle).await?,
        ProxyMessage::Busy {
            sessions: vec![one]
        }
    );
    assert!(!proxy.task.is_finished());

    // Without a session, the proxy stops right away instead of after its idle timeout.
    drop(registrar);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while ask(&proxy.paths, &ClientMessage::Hello).await?
        != (ProxyMessage::Info {
            port: proxy.port,
            pid: std::process::id(),
            protocol: PROTOCOL_VERSION,
            sessions: Some(Vec::new()),
        })
    {
        eyre::ensure!(
            tokio::time::Instant::now() < deadline,
            "the session stayed listed"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        ask(&proxy.paths, &ClientMessage::StopIfIdle).await?,
        ProxyMessage::Stopping
    );
    tokio::time::timeout(Duration::from_secs(5), proxy.task).await???;
    assert!(!proxy.paths.socket.exists());
    Ok(())
}

/// A WebSocket-style upgrade becomes a two-way byte tunnel once the upstream switches protocols.
#[tokio::test]
async fn upgrades_become_tunnels() -> eyre::Result<()> {
    let proxy = start_proxy(Duration::from_secs(60)).await?;
    let upstream = TcpListener::bind(("127.0.0.1", 0)).await?;
    let upstream_port = upstream.local_addr()?.port();
    let _registrar = Registrar::start(
        proxy.paths.clone(),
        registration("one", &[("ws", upstream_port, None)]),
        proxy.port,
        None,
        &CancellationToken::new(),
        |_| {},
    )
    .await?;

    // An echo server behind an upgrade handshake
    let server = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await?;
        let head = read_head(&mut stream).await?;
        stream
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n",
            )
            .await?;
        let mut buffer = [0u8; 64];
        loop {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            stream
                .write_all(buffer.get(..read).unwrap_or_default())
                .await?;
        }
        Ok::<_, eyre::Report>(head)
    });

    let mut client = TcpStream::connect(("127.0.0.1", proxy.port)).await?;
    client
        .write_all(b"GET /socket HTTP/1.1\r\nHost: ws.one.localhost\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n")
        .await?;
    let response = read_head(&mut client).await?;
    assert!(response.starts_with("HTTP/1.1 101"), "{response}");
    assert!(
        response.to_ascii_lowercase().contains("upgrade: echo"),
        "{response}"
    );

    client.write_all(b"ping").await?;
    let mut echoed = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut echoed)).await??;
    assert_eq!(&echoed, b"ping");

    drop(client);
    let head = server.await??;
    // The upstream was asked for the same upgrade.
    assert!(
        head.to_ascii_lowercase().contains("upgrade: echo"),
        "{head}"
    );

    proxy.shutdown.cancel();
    proxy.task.await??;
    Ok(())
}

/// Unknown hosts list the live sessions, and a route whose upstream is down names its session and
/// service.
#[tokio::test]
async fn error_pages_name_sessions_and_services() -> eyre::Result<()> {
    let proxy = start_proxy(Duration::from_secs(60)).await?;
    let (_closed, closed_port) = refusing_port()?;
    let _registrar = Registrar::start(
        proxy.paths.clone(),
        registration("one", &[("api", closed_port, Some("api-service"))]),
        proxy.port,
        None,
        &CancellationToken::new(),
        |_| {},
    )
    .await?;

    let missing = get(proxy.port, "nope.localhost").await?;
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    assert!(
        missing.contains(&format!("http://api.one.localhost:{}/", proxy.port)),
        "{missing}"
    );

    let down = get(proxy.port, "api.one.localhost").await?;
    assert!(down.starts_with("HTTP/1.1 502"), "{down}");
    assert!(down.contains("<strong>api-service</strong>"), "{down}");
    assert!(down.contains("<strong>one</strong>"), "{down}");

    proxy.shutdown.cancel();
    proxy.task.await??;
    Ok(())
}

/// Opens an upgraded tunnel to `host` through the proxy, served by an echo server on `upstream`.
async fn open_echo_tunnel(
    proxy_port: u16,
    host: &str,
    upstream: TcpListener,
) -> eyre::Result<(TcpStream, tokio::task::JoinHandle<eyre::Result<()>>)> {
    let echo = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await?;
        read_head(&mut stream).await?;
        stream
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n",
            )
            .await?;
        let mut buffer = [0u8; 64];
        loop {
            let read = stream.read(&mut buffer).await?;
            if read == 0 {
                return Ok(());
            }
            stream
                .write_all(buffer.get(..read).unwrap_or_default())
                .await?;
        }
    });
    let mut tunnel = TcpStream::connect(("127.0.0.1", proxy_port)).await?;
    tunnel
        .write_all(
            format!(
                "GET /socket HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: echo\r\n\r\n"
            )
            .as_bytes(),
        )
        .await?;
    let switched = read_head(&mut tunnel).await?;
    eyre::ensure!(switched.starts_with("HTTP/1.1 101"), "{switched}");
    Ok((tunnel, echo))
}

/// Replacing a live registration's routes takes effect without a reconnect: an added route is
/// served, a removed one is unknown, a route that moved names its new service, and a tunnel the
/// session already had stays open.
#[tokio::test]
async fn replaced_routes_take_effect_on_the_live_registration() -> eyre::Result<()> {
    let proxy = start_proxy(Duration::from_secs(60)).await?;
    let admin_upstream = TcpListener::bind(("127.0.0.1", 0)).await?;
    let admin_port = admin_upstream.local_addr()?.port();
    let echo_upstream = TcpListener::bind(("127.0.0.1", 0)).await?;
    let echo_port = echo_upstream.local_addr()?.port();
    // Ports that refuse connections, so each route answers with its 502 page
    let (_app, app_port) = refusing_port()?;
    let (_removed, removed_port) = refusing_port()?;
    let (_moved, moved_port) = refusing_port()?;
    let registrar = Registrar::start(
        proxy.paths.clone(),
        registration(
            "one",
            &[
                ("app", app_port, Some("web")),
                ("api", removed_port, Some("api")),
                ("ws", echo_port, None),
            ],
        ),
        proxy.port,
        None,
        &CancellationToken::new(),
        |_| {},
    )
    .await?;
    let mut status = registrar.status();
    status.borrow_and_update();
    let before = get(proxy.port, "app.one.localhost").await?;
    assert!(before.contains("<strong>web</strong>"), "{before}");

    // An echo tunnel that is open while the routes change
    let (mut tunnel, echo) =
        open_echo_tunnel(proxy.port, "ws.one.localhost", echo_upstream).await?;

    // `app` moves to `frontend` on another port, `api` goes, and `admin` comes.
    let replaced = registration(
        "one",
        &[
            ("app", moved_port, Some("frontend")),
            ("admin", admin_port, Some("admin")),
            ("ws", echo_port, None),
        ],
    );
    registrar.routes().send_replace(replaced.routes);

    // The proxy applies the replacement asynchronously, so wait for the moved route.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let moved = loop {
        let response = get(proxy.port, "app.one.localhost").await?;
        if response.contains("<strong>frontend</strong>") {
            break response;
        }
        eyre::ensure!(
            tokio::time::Instant::now() < deadline,
            "the route never moved: {response}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(moved.starts_with("HTTP/1.1 502"), "{moved}");

    // The removed route is unknown now, and the added one reaches its upstream.
    let removed = get(proxy.port, "api.one.localhost").await?;
    assert!(removed.starts_with("HTTP/1.1 404"), "{removed}");
    let admin = tokio::spawn(async move {
        let (mut stream, _) = admin_upstream.accept().await?;
        let head = read_head(&mut stream).await?;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await?;
        Ok::<_, eyre::Report>(head)
    });
    let response = get(proxy.port, "admin.one.localhost").await?;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let head = admin.await??;
    assert!(head.contains("admin.one.localhost"), "{head}");

    // The open tunnel still echoes, and the registration never dropped to reconnect.
    tunnel.write_all(b"ping").await?;
    let mut echoed = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), tunnel.read_exact(&mut echoed)).await??;
    assert_eq!(&echoed, b"ping");
    assert!(!status.has_changed()?, "the registration reconnected");

    drop(tunnel);
    echo.await??;
    proxy.shutdown.cancel();
    proxy.task.await??;
    Ok(())
}

/// A session's routes vanish with its registration, and the proxy exits once it has been idle.
///
/// The runtime's clock is paused, so a slow machine cannot run the idle timeout out before the
/// session registers.
/// Whenever every task waits, the paused clock jumps to the next timer, and the proxy's idle
/// tick of at most a second bounds each jump.
/// A one-minute idle timeout therefore leaves the registration handshake dozens of round trips,
/// while the test still finishes in milliseconds.
#[tokio::test(start_paused = true)]
async fn routes_end_with_the_registration_and_the_proxy_exits_when_idle() -> eyre::Result<()> {
    const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
    let proxy = start_proxy(IDLE_TIMEOUT).await?;
    let upstream = TcpListener::bind(("127.0.0.1", 0)).await?;
    let registrar = Registrar::start(
        proxy.paths.clone(),
        registration("one", &[("app", upstream.local_addr()?.port(), None)]),
        proxy.port,
        None,
        &CancellationToken::new(),
        |_| {},
    )
    .await?;

    // A registered session keeps the proxy alive past its idle timeout.
    tokio::time::sleep(IDLE_TIMEOUT * 2).await;
    assert!(!proxy.task.is_finished());

    drop(registrar);
    // The proxy notices the closed connection and later exits on its own.
    tokio::time::timeout(IDLE_TIMEOUT * 5, proxy.task).await???;
    assert!(!proxy.paths.socket.exists());
    Ok(())
}

/// A second session for the same session domain is refused, and only an acceptable running proxy
/// is joined.
#[tokio::test]
async fn conflicting_sessions_and_ports_are_refused() -> eyre::Result<()> {
    let proxy = start_proxy(Duration::from_secs(60)).await?;
    let _first = Registrar::start(
        proxy.paths.clone(),
        registration("one", &[("app", 1, None)]),
        proxy.port,
        None,
        &CancellationToken::new(),
        |_| {},
    )
    .await?;
    let mut other = registration("one", &[("app", 2, None)]);
    other.config_path = "/elsewhere/micromux.yaml".to_string();

    let conflict = Registrar::start(
        proxy.paths.clone(),
        other,
        proxy.port,
        None,
        &CancellationToken::new(),
        |_| {},
    )
    .await;
    assert!(
        matches!(conflict, Err(micromux_proxy::ClientError::Rejected(_))),
        "{conflict:?}"
    );

    let joined = ensure_proxy(
        &proxy.paths,
        PortPreference {
            port: 1,
            fallback_port: Some(proxy.port),
        },
        None,
    )
    .await?;
    assert_eq!(joined, proxy.port);
    let mismatch = ensure_proxy(
        &proxy.paths,
        PortPreference {
            port: 1,
            fallback_port: None,
        },
        None,
    )
    .await;
    assert!(
        matches!(
            mismatch,
            Err(micromux_proxy::ClientError::PortMismatch { .. })
        ),
        "{mismatch:?}"
    );

    proxy.shutdown.cancel();
    proxy.task.await??;
    Ok(())
}

/// A streaming response reaches the client chunk by chunk, before the upstream finishes it.
#[tokio::test]
async fn streaming_responses_are_not_buffered() -> eyre::Result<()> {
    let proxy = start_proxy(Duration::from_secs(60)).await?;
    let upstream = TcpListener::bind(("127.0.0.1", 0)).await?;
    let upstream_port = upstream.local_addr()?.port();
    // Through a stripped path route, which rewrites only the request target
    let mut one = registration("one", &[]);
    one.routes
        .push(path_route("events", "/events/", true, upstream_port));
    let _registrar = Registrar::start(
        proxy.paths.clone(),
        one,
        proxy.port,
        None,
        &CancellationToken::new(),
        |_| {},
    )
    .await?;

    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await?;
        let head = read_head(&mut stream).await?;
        eyre::ensure!(head.starts_with("GET /stream HTTP/1.1\r\n"), "{head}");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n6\r\nfirst\n\r\n")
            .await?;
        // Hold the stream open until the client proved it already got the first event.
        finish_rx.await?;
        stream.write_all(b"5\r\nlast\n\r\n0\r\n\r\n").await?;
        Ok::<_, eyre::Report>(())
    });

    let mut client = TcpStream::connect(("127.0.0.1", proxy.port)).await?;
    client
        .write_all(
            b"GET /events/stream HTTP/1.1\r\nHost: events.one.localhost\r\nConnection: close\r\n\r\n",
        )
        .await?;
    let mut received = Vec::new();
    let mut buffer = [0u8; 256];
    while !String::from_utf8_lossy(&received).contains("first") {
        let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buffer)).await??;
        eyre::ensure!(read > 0, "the stream ended before the first event");
        received.extend_from_slice(buffer.get(..read).unwrap_or_default());
    }
    finish_tx
        .send(())
        .map_err(|()| eyre::eyre!("the upstream is gone"))?;
    let mut rest = String::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_string(&mut rest)).await??;
    assert!(rest.contains("last"), "{rest}");
    server.await??;

    proxy.shutdown.cancel();
    proxy.task.await??;
    Ok(())
}

/// A chunked request body of unknown length reaches the upstream intact, whatever the method.
#[tokio::test]
async fn chunked_request_bodies_are_forwarded() -> eyre::Result<()> {
    let proxy = start_proxy(Duration::from_secs(60)).await?;
    let upstream = TcpListener::bind(("127.0.0.1", 0)).await?;
    let upstream_port = upstream.local_addr()?.port();
    // Through a path route, which leaves the body alone
    let mut one = registration("one", &[]);
    one.routes
        .push(path_route("search", "/_search", false, upstream_port));
    let _registrar = Registrar::start(
        proxy.paths.clone(),
        one,
        proxy.port,
        None,
        &CancellationToken::new(),
        |_| {},
    )
    .await?;

    let server = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await?;
        let head = read_head(&mut stream).await?;
        // Read the chunked body up to its terminating chunk.
        let mut body = Vec::new();
        let mut byte = [0u8; 1];
        while !body.ends_with(b"0\r\n\r\n") {
            if stream.read(&mut byte).await? == 0 {
                break;
            }
            body.push(byte[0]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await?;
        Ok::<_, eyre::Report>((head, String::from_utf8(body)?))
    });

    let mut client = TcpStream::connect(("127.0.0.1", proxy.port)).await?;
    client
        .write_all(
            b"GET /_search HTTP/1.1\r\nHost: search.one.localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n9\r\n{\"q\":\"x\"}\r\n0\r\n\r\n",
        )
        .await?;
    let mut response = String::new();
    tokio::time::timeout(
        Duration::from_secs(10),
        client.read_to_string(&mut response),
    )
    .await??;
    let (head, body) = server.await??;

    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(
        head.to_ascii_lowercase()
            .contains("transfer-encoding: chunked"),
        "{head}"
    );
    assert!(body.contains("{\"q\":\"x\"}"), "{body}");

    proxy.shutdown.cancel();
    proxy.task.await??;
    Ok(())
}

/// Stopping the proxy ends every registration, so a registered session notices at once instead
/// of staying attached to a proxy that no longer routes anything.
#[tokio::test]
async fn stopping_the_proxy_ends_its_registrations() -> eyre::Result<()> {
    let proxy = start_proxy(Duration::from_secs(60)).await?;
    let registrar = Registrar::start(
        proxy.paths.clone(),
        registration("one", &[("app", 1, None)]),
        proxy.port,
        None,
        &CancellationToken::new(),
        |_| {},
    )
    .await?;
    let mut status = registrar.status();

    proxy.shutdown.cancel();
    proxy.task.await??;

    tokio::time::timeout(
        Duration::from_secs(5),
        status.wait_for(|status| {
            matches!(
                status,
                micromux_proxy::RegistrationStatus::Unavailable { .. }
            )
        }),
    )
    .await??;
    Ok(())
}

/// A stopped proxy has released its port by the time `serve` returns, so a successor can bind it
/// right away.
#[tokio::test(flavor = "current_thread")]
async fn a_stopped_proxy_frees_its_port_immediately() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let paths = ProxyPaths::in_dir(directory.path());
    let mut options = ServeOptions {
        paths,
        preference: PortPreference {
            port: 0,
            fallback_port: None,
        },
        idle_timeout: Duration::from_secs(60),
    };

    // The first proxy picks the port itself, so no probe listener frees it for a parallel test
    // to take before the proxy binds it.
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    let (port_tx, port_rx) = tokio::sync::oneshot::channel();
    serve(options.clone(), shutdown, move |port| {
        let _ = port_tx.send(port);
        stop.cancel();
    })
    .await?;
    let port = port_rx.await?;

    // A successor on that very port binds it as soon as the first proxy has returned.
    options.preference.port = port;
    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    serve(options, shutdown, move |_| stop.cancel()).await?;
    Ok(())
}
