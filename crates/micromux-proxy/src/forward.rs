//! Forwarding one HTTP connection to the routes it names.
//!
//! Every request is looked up again by its `Host` header and path and sent over a fresh upstream
//! connection, so a keep-alive browser connection never sticks to a route that has gone away.
//! Bodies stream in both directions without buffering, and an upgrade (WebSocket) becomes a
//! byte tunnel once the upstream answers `101 Switching Protocols`.

use std::convert::Infallible;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode, body::Incoming};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use crate::pages;
use crate::server::Shared;

/// How long the proxy waits for a local upstream to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

type Body = BoxBody<Bytes, hyper::Error>;

/// Serves one downstream connection, accepted on proxy port `port`, until the client closes it.
pub(crate) async fn serve_connection(stream: TcpStream, port: u16, shared: Arc<Shared>) {
    let _ = stream.set_nodelay(true);
    let service = hyper::service::service_fn(move |request| {
        let shared = shared.clone();
        async move { Ok::<_, Infallible>(handle(request, port, &shared).await) }
    });
    if let Err(err) = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades()
        .await
    {
        tracing::debug!(%err, "downstream connection ended with an error");
    }
}

async fn handle(mut request: Request<Incoming>, port: u16, shared: &Shared) -> Response<Body> {
    let Some(host) = request_host(&request) else {
        return text(StatusCode::BAD_REQUEST, "the request names no host");
    };
    let Some(target) = shared.lookup(port, &host, request.uri().path()) else {
        return html(
            StatusCode::NOT_FOUND,
            pages::not_found(
                &host,
                request.uri().path(),
                &shared.listed_sessions_with_git().await,
            ),
        );
    };
    let route = format!("{host}{}", target.path.as_deref().unwrap_or_default());
    let bad_gateway = |error: &str| {
        html(
            StatusCode::BAD_GATEWAY,
            pages::bad_gateway(
                &target.session,
                target.service.as_deref(),
                &route,
                target.port,
                error,
            ),
        )
    };
    let origin_form = upstream_target(
        request.uri(),
        target.path.as_deref().filter(|_| target.strip_prefix),
    );
    let Ok(origin_form) = origin_form.parse() else {
        return text(StatusCode::BAD_REQUEST, "the request target is invalid");
    };

    // Connect upstream
    let stream = match tokio::time::timeout(
        CONNECT_TIMEOUT,
        TcpStream::connect((Ipv4Addr::LOCALHOST, target.port)),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(err)) => return bad_gateway(&err.to_string()),
        Err(_) => return bad_gateway("the connection timed out"),
    };
    let _ = stream.set_nodelay(true);
    let (mut sender, connection) = match hyper::client::conn::http1::Builder::new()
        .handshake::<_, Incoming>(TokioIo::new(stream))
        .await
    {
        Ok(handshake) => handshake,
        Err(err) => return bad_gateway(&err.to_string()),
    };
    // The upstream connection carries the response body after this function returns; it ends
    // with the session's registration.
    let registration = target.cancel.clone();
    tokio::spawn(async move {
        tokio::select! {
            () = registration.cancelled() => {}
            result = connection.with_upgrades() => {
                if let Err(err) = result {
                    tracing::debug!(%err, "upstream connection ended with an error");
                }
            }
        }
    });

    // Rewrite the request for the upstream hop
    let upgrade = requested_upgrade(request.headers());
    let downstream_upgrade = upgrade.is_some().then(|| hyper::upgrade::on(&mut request));
    if !request.headers().contains_key(header::HOST)
        && let Ok(value) = HeaderValue::from_str(&host)
    {
        request.headers_mut().insert(header::HOST, value);
    }
    strip_hop_by_hop(request.headers_mut());
    if let Some(protocol) = &upgrade {
        mark_upgrade(request.headers_mut(), protocol.clone());
    }
    *request.uri_mut() = origin_form;

    let mut response = match sender.send_request(request).await {
        Ok(response) => response,
        Err(err) => return bad_gateway(&err.to_string()),
    };

    // Upgrade: tunnel bytes once both sides switched protocols
    if response.status() == StatusCode::SWITCHING_PROTOCOLS
        && let Some(downstream) = downstream_upgrade
    {
        let protocol = response.headers().get(header::UPGRADE).cloned();
        let upstream = hyper::upgrade::on(&mut response);
        tokio::spawn(tunnel(downstream, upstream, target.cancel.clone()));
        strip_hop_by_hop(response.headers_mut());
        if let Some(protocol) = protocol {
            mark_upgrade(response.headers_mut(), protocol);
        }
        return response.map(|_| empty());
    }

    strip_hop_by_hop(response.headers_mut());
    response.map(BodyExt::boxed)
}

/// The origin-form target the upstream receives for `uri`: its path and query, with `strip`
/// removed from the front of the path when given.
///
/// The path never ends up empty or relative: what remains after stripping gets a leading `/`.
fn upstream_target(uri: &hyper::Uri, strip: Option<&str>) -> String {
    let path = uri.path();
    let path = match strip.and_then(|prefix| path.strip_prefix(prefix)) {
        Some(rest) if rest.starts_with('/') => rest.to_string(),
        Some(rest) => format!("/{rest}"),
        None if path.is_empty() => "/".to_string(),
        None => path.to_string(),
    };
    match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    }
}

/// Splices an upgraded connection to its upstream until either side closes or the session's
/// registration ends.
async fn tunnel(
    downstream: hyper::upgrade::OnUpgrade,
    upstream: hyper::upgrade::OnUpgrade,
    registration: CancellationToken,
) {
    let (downstream, upstream) = match tokio::try_join!(downstream, upstream) {
        Ok(upgraded) => upgraded,
        Err(err) => {
            tracing::debug!(%err, "upgrade failed");
            return;
        }
    };
    let mut downstream = TokioIo::new(downstream);
    let mut upstream = TokioIo::new(upstream);
    tokio::select! {
        () = registration.cancelled() => {}
        result = tokio::io::copy_bidirectional(&mut downstream, &mut upstream) => {
            if let Err(err) = result {
                tracing::debug!(%err, "upgraded connection ended with an error");
            }
        }
    }
}

/// The host a request is for, lowercased, without port or trailing dot.
fn request_host<B>(request: &Request<B>) -> Option<String> {
    let raw = match request.headers().get(header::HOST) {
        Some(value) => value.to_str().ok()?.to_string(),
        None => request.uri().host()?.to_string(),
    };
    Some(normalize_host(&raw))
}

fn normalize_host(raw: &str) -> String {
    let raw = raw.trim();
    let host = if raw.starts_with('[') {
        // An IPv6 literal keeps its brackets; only a port after `]` is stripped.
        raw.split_once(']')
            .map_or(raw, |(address, _)| address)
            .to_string()
            + "]"
    } else {
        match raw.rsplit_once(':') {
            Some((host, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => {
                host.to_string()
            }
            _ => raw.to_string(),
        }
    };
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// The protocol a request asks to upgrade to, when it does.
fn requested_upgrade(headers: &HeaderMap) -> Option<HeaderValue> {
    connection_tokens(headers)
        .iter()
        .any(|token| token == "upgrade")
        .then(|| headers.get(header::UPGRADE).cloned())
        .flatten()
}

fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|token| token.trim().to_ascii_lowercase())
        .filter(|token| !token.is_empty())
        .collect()
}

/// Removes the fields that describe one connection rather than the message, as an intermediary
/// must ([RFC 9110 section 7.6.1][rfc]).
///
/// `Transfer-Encoding` and `Trailer` stay: hyper re-frames every hop from them, and without
/// `Transfer-Encoding` it would send a chunked request body of unknown length as empty and drop
/// declared trailers.
///
/// [rfc]: https://www.rfc-editor.org/rfc/rfc9110.html#section-7.6.1
fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for token in connection_tokens(headers) {
        if let Ok(name) = HeaderName::from_bytes(token.as_bytes()) {
            headers.remove(name);
        }
    }
    for name in [
        header::CONNECTION,
        HeaderName::from_static("keep-alive"),
        HeaderName::from_static("proxy-connection"),
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::UPGRADE,
    ] {
        headers.remove(name);
    }
}

fn mark_upgrade(headers: &mut HeaderMap, protocol: HeaderValue) {
    headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(header::UPGRADE, protocol);
}

fn empty() -> Body {
    Empty::new().map_err(|never| match never {}).boxed()
}

fn html(status: StatusCode, page: String) -> Response<Body> {
    with_body(status, "text/html; charset=utf-8", page)
}

fn text(status: StatusCode, message: &str) -> Response<Body> {
    with_body(status, "text/plain; charset=utf-8", message.to_string())
}

fn with_body(status: StatusCode, content_type: &'static str, body: String) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::from(body))
            .map_err(|never| match never {})
            .boxed(),
    );
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use similar_asserts::assert_eq;

    #[test]
    fn hosts_are_normalized() {
        assert_eq!(
            normalize_host("App.Luup2.localhost:8080"),
            "app.luup2.localhost"
        );
        assert_eq!(
            normalize_host("app.luup2.localhost."),
            "app.luup2.localhost"
        );
        assert_eq!(normalize_host("[::1]:80"), "[::1]");
        assert_eq!(normalize_host("localhost"), "localhost");
    }

    /// Fields a client names in `Connection` describe its hop only and never reach the upstream.
    #[test]
    fn hop_by_hop_fields_are_removed() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, X-Secret"),
        );
        headers.insert("x-secret", HeaderValue::from_static("1"));
        headers.insert(header::TE, HeaderValue::from_static("trailers"));
        headers.insert(header::HOST, HeaderValue::from_static("app.one.localhost"));

        strip_hop_by_hop(&mut headers);

        assert_eq!(headers.len(), 1);
        assert!(headers.contains_key(header::HOST));
    }

    /// Stripping removes exactly the matched prefix, keeps the query, and always leaves an
    /// absolute path; without stripping the target passes through unchanged.
    #[test]
    fn upstream_targets_strip_the_prefix_and_keep_the_query() {
        let target = |uri: &str, strip: Option<&str>| {
            uri.parse::<hyper::Uri>()
                .map(|uri| upstream_target(&uri, strip))
                .unwrap_or_default()
        };

        // Stripped
        assert_eq!(
            target("/auth/v1/token?x=1", Some("/auth/v1/")),
            "/token?x=1"
        );
        assert_eq!(target("/auth/v1/token", Some("/auth/v1")), "/token");
        assert_eq!(target("/auth/v1/", Some("/auth/v1/")), "/");
        assert_eq!(target("/auth/v1?x=1", Some("/auth/v1")), "/?x=1");
        assert_eq!(target("/auth/v1x", Some("/auth/v1")), "/x");
        assert_eq!(target("/auth/v1//x", Some("/auth/v1/")), "/x");
        assert_eq!(target("/auth//x", Some("/auth")), "//x");
        // A leading `//` stays a path rather than turning into an authority.
        let reparsed = "//x".parse::<hyper::Uri>().ok();
        assert_eq!(
            reparsed.map(|uri| (uri.authority().is_none(), uri.path().to_string())),
            Some((true, "//x".to_string()))
        );

        // Unchanged
        assert_eq!(target("/rest/x?y", None), "/rest/x?y");
        assert_eq!(target("http://app.one.localhost/a?b", None), "/a?b");
        assert_eq!(target("http://app.one.localhost", None), "/");
    }

    #[test]
    fn upgrade_needs_both_connection_token_and_upgrade_field() {
        let mut headers = HeaderMap::new();
        headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        assert_eq!(requested_upgrade(&headers), None);
        headers.insert(
            header::CONNECTION,
            HeaderValue::from_static("keep-alive, Upgrade"),
        );
        assert_eq!(
            requested_upgrade(&headers),
            Some(HeaderValue::from_static("websocket"))
        );
    }
}
