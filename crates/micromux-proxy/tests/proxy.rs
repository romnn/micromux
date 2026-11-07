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

/// A proxy that does not report the path-route protocol, as one started by an older micromux,
/// would ignore route paths, so a session with path routes refuses it with an error naming the
/// proxy, while a session without paths still registers until its routes gain a path.
#[tokio::test]
async fn path_routes_refuse_an_outdated_proxy() -> eyre::Result<()> {
    use micromux_proxy::protocol::{ClientMessage, ProxyMessage};

    // A stand-in for an older proxy: it answers `hello` without a protocol and accepts every
    // registration.
    let directory = tempfile::tempdir()?;
    let paths = ProxyPaths::in_dir(directory.path());
    let listener = tokio::net::UnixListener::bind(&paths.socket)?;
    let old_proxy = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut lines = tokio::io::BufReader::new(reader);
                let mut line = String::new();
                while tokio::io::AsyncBufReadExt::read_line(&mut lines, &mut line)
                    .await
                    .is_ok_and(|read| read > 0)
                {
                    let reply = match serde_json::from_str::<ClientMessage>(&line) {
                        Ok(ClientMessage::Hello) => r#"{"type":"info","port":1,"pid":4242}"#,
                        Ok(ClientMessage::Register(_)) => r#"{"type":"registered"}"#,
                        _ => r#"{"type":"pong"}"#,
                    };
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
    // The stand-in's reply is what an older proxy sends: no protocol, read as the first one.
    assert!(matches!(
        serde_json::from_str::<ProxyMessage>(r#"{"type":"info","port":1,"pid":4242}"#)?,
        ProxyMessage::Info { protocol: 1, .. }
    ));

    let mut with_paths = registration("one", &[]);
    with_paths
        .routes
        .push(path_route("api", "/rest/", false, 2));
    let refused = Registrar::start(
        paths.clone(),
        with_paths,
        1,
        None,
        &CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(micromux_proxy::ClientError::OutdatedProxy {
                pid: 4242,
                protocol: 1,
                required: 2,
            })
        ),
        "{refused:?}"
    );

    let without_paths = Registrar::start(
        paths,
        registration("two", &[("app", 3, None)]),
        1,
        None,
        &CancellationToken::new(),
    )
    .await?;

    // Routes that gain a path later are not sent over the connection checked without one; the
    // reconnect refuses the old proxy and reports why.
    let mut status = without_paths.status();
    without_paths
        .routes()
        .send_replace(vec![path_route("app", "/rest/", false, 3)]);
    tokio::time::timeout(
        Duration::from_secs(5),
        status.wait_for(|status| {
            matches!(
                status,
                micromux_proxy::RegistrationStatus::Unavailable { reason }
                    if reason.contains("registration protocol 1")
            )
        }),
    )
    .await??;

    old_proxy.abort();
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
