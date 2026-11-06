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
                port: *port,
                service: service.map(ToString::to_string),
            })
            .collect(),
    }
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
    // A port nothing listens on
    let closed_port = TcpListener::bind(("127.0.0.1", 0))
        .await?
        .local_addr()?
        .port();
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

/// A session's routes vanish with its registration, and the proxy exits once it has been idle.
#[tokio::test]
async fn routes_end_with_the_registration_and_the_proxy_exits_when_idle() -> eyre::Result<()> {
    let proxy = start_proxy(Duration::from_millis(300)).await?;
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
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!proxy.task.is_finished());

    drop(registrar);
    // The proxy notices the closed connection and later exits on its own.
    tokio::time::timeout(Duration::from_secs(5), proxy.task).await???;
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
    let _registrar = Registrar::start(
        proxy.paths.clone(),
        registration("one", &[("events", upstream_port, None)]),
        proxy.port,
        None,
        &CancellationToken::new(),
    )
    .await?;

    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await?;
        read_head(&mut stream).await?;
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
            b"GET /events HTTP/1.1\r\nHost: events.one.localhost\r\nConnection: close\r\n\r\n",
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
    let _registrar = Registrar::start(
        proxy.paths.clone(),
        registration("one", &[("search", upstream_port, None)]),
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
    let port = TcpListener::bind(("127.0.0.1", 0))
        .await?
        .local_addr()?
        .port();
    let options = ServeOptions {
        paths,
        preference: PortPreference {
            port,
            fallback_port: None,
        },
        idle_timeout: Duration::from_secs(60),
    };

    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    serve(options.clone(), shutdown, move |_| stop.cancel()).await?;

    let shutdown = CancellationToken::new();
    let stop = shutdown.clone();
    serve(options, shutdown, move |_| stop.cancel()).await?;
    Ok(())
}
