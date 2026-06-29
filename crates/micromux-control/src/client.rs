//! The control client used by `micromux ctl` and the MCP proxy.
//!
//! The proxy never mutates the filesystem; it only connects. Discovery skips endpoints that refuse
//! a connection (dead), surfaces busy/incompatible endpoints, and never prunes them.

use std::path::Path;

use crate::ControlError;
use crate::endpoint::ControlEndpoint;
use crate::protocol::{PROTOCOL_VERSION, Request, Response, SessionInfo};
#[cfg(unix)]
use crate::{REQUEST_TIMEOUT, read_message, write_message};

/// A connected control client.
pub struct Client {
    #[cfg(unix)]
    conn: crate::Framing<tokio::net::UnixStream>,
}

/// A streaming subscription to liveness-only session changes.
pub struct Subscription {
    #[cfg(unix)]
    conn: crate::Framing<tokio::net::UnixStream>,
}

/// A live session found by [`discover_sessions`].
#[derive(Debug, Clone)]
pub struct DiscoveredSession {
    /// The session's live identity.
    pub info: SessionInfo,
    /// The endpoint to reach it.
    pub endpoint: ControlEndpoint,
}

/// The result of scanning for live sessions: those that answered `Describe`, plus a count of
/// live-but-unreachable endpoints (busy/timeout/incompatible) that were skipped this scan. A
/// non-zero `unreachable` means a target *might* be present but could not be described — it is never
/// de-listed or pruned (the proxy only reads).
#[derive(Debug, Default)]
pub struct Discovery {
    /// Sessions that answered `Describe`.
    pub sessions: Vec<DiscoveredSession>,
    /// Number of live endpoints that could not be described this scan (busy/incompatible).
    pub unreachable: usize,
}

impl Client {
    /// Connect to a control endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError::Io`] if the endpoint refuses the connection (dead session) or
    /// [`ControlError::Unsupported`] on a platform without a transport.
    #[cfg_attr(not(unix), allow(unused_variables, clippy::unused_async))]
    pub async fn connect(endpoint: &ControlEndpoint) -> Result<Self, ControlError> {
        match endpoint {
            #[cfg(unix)]
            ControlEndpoint::Unix(path) => {
                let stream =
                    tokio::time::timeout(REQUEST_TIMEOUT, tokio::net::UnixStream::connect(path))
                        .await
                        .map_err(|_| ControlError::Timeout)??;
                Ok(Self {
                    conn: crate::framed(stream),
                })
            }
            #[cfg(not(unix))]
            ControlEndpoint::Unix(_) => Err(ControlError::Unsupported),
            ControlEndpoint::WindowsNamedPipe(_) => Err(ControlError::Unsupported),
        }
    }

    /// Send one request and await one response, bounded by [`REQUEST_TIMEOUT`].
    ///
    /// # Errors
    ///
    /// Returns [`ControlError::Timeout`] if the peer does not reply in time, [`ControlError::Closed`]
    /// if the connection ends first, or an I/O/serialization error.
    #[cfg_attr(not(unix), allow(unused_variables, clippy::unused_async))]
    pub async fn request(&mut self, request: Request) -> Result<Response, ControlError> {
        #[cfg(unix)]
        {
            write_message(&mut self.conn, &request).await?;
            let read =
                tokio::time::timeout(REQUEST_TIMEOUT, read_message::<_, Response>(&mut self.conn))
                    .await
                    .map_err(|_| ControlError::Timeout)?;
            match read? {
                Some(response) => Ok(response),
                None => Err(ControlError::Closed),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = request;
            Err(ControlError::Unsupported)
        }
    }

    /// Subscribe to liveness-only [`micromux::SessionChange`] notifications from an endpoint.
    ///
    /// # Errors
    ///
    /// Returns a transport/protocol error if the endpoint cannot be reached or rejects the
    /// subscription.
    #[cfg_attr(not(unix), allow(unused_variables, clippy::unused_async))]
    pub async fn subscribe(endpoint: &ControlEndpoint) -> Result<Subscription, ControlError> {
        #[cfg(unix)]
        {
            let ControlEndpoint::Unix(path) = endpoint else {
                return Err(ControlError::Unsupported);
            };
            let stream =
                tokio::time::timeout(REQUEST_TIMEOUT, tokio::net::UnixStream::connect(path))
                    .await
                    .map_err(|_| ControlError::Timeout)??;
            let mut conn = crate::framed(stream);
            write_message(&mut conn, &Request::Subscribe).await?;
            Ok(Subscription { conn })
        }

        #[cfg(not(unix))]
        {
            let _ = endpoint;
            Err(ControlError::Unsupported)
        }
    }

    /// Fetch and version-check the session identity.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError::ProtocolMismatch`] on a version mismatch, or [`ControlError::Unexpected`]
    /// if the peer replies with something other than a description.
    pub async fn describe(&mut self) -> Result<SessionInfo, ControlError> {
        match self.request(Request::Describe).await? {
            Response::Description(info) => {
                if info.protocol_version == PROTOCOL_VERSION {
                    Ok(info)
                } else {
                    Err(ControlError::ProtocolMismatch {
                        peer: info.protocol_version,
                        ours: PROTOCOL_VERSION,
                    })
                }
            }
            other => Err(ControlError::Unexpected(format!("{other:?}"))),
        }
    }
}

impl Subscription {
    /// Receive the next change notification, or `Ok(None)` if the session closed the stream.
    ///
    /// # Errors
    ///
    /// Returns a transport/protocol error if the stream is malformed.
    #[cfg_attr(not(unix), allow(clippy::unused_async))]
    pub async fn recv(&mut self) -> Result<Option<micromux::SessionChange>, ControlError> {
        #[cfg(unix)]
        {
            match read_message::<_, Response>(&mut self.conn).await? {
                Some(Response::Change(change)) => Ok(Some(change)),
                Some(Response::Error { code, message }) => Err(ControlError::Unexpected(format!(
                    "subscription error {code:?}: {message}"
                ))),
                Some(other) => Err(ControlError::Unexpected(format!("{other:?}"))),
                None => Ok(None),
            }
        }

        #[cfg(not(unix))]
        {
            Err(ControlError::Unsupported)
        }
    }
}

/// Discover every live session by scanning the runtime dir's `*.sock` endpoints, connecting, and
/// calling `Describe`.
///
/// Hard-dead endpoints (gone/refused) are skipped. A live-but-unreachable endpoint
/// (busy/timeout/incompatible) is *counted* in [`Discovery::unreachable`] rather than aborting the
/// scan — so one wedged session never de-lists the healthy ones, and a busy target is never silently
/// reported as absent.
///
/// # Errors
///
/// Returns [`ControlError::Unsupported`] on a platform without a transport. Per-endpoint failures do
/// not error the scan.
pub async fn discover_sessions(runtime_dir: &Path) -> Result<Discovery, ControlError> {
    #[cfg(not(unix))]
    {
        let _ = runtime_dir;
        return Err(ControlError::Unsupported);
    }

    #[cfg(unix)]
    {
        let mut discovery = Discovery::default();
        let Ok(entries) = std::fs::read_dir(runtime_dir) else {
            return Ok(discovery);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("sock") {
                continue;
            }
            match try_describe(&ControlEndpoint::Unix(path)).await {
                Ok(Some(session)) => discovery.sessions.push(session),
                Ok(None) => {} // hard-dead (gone/refused): skip
                Err(err) => {
                    tracing::debug!(?err, "control: live endpoint unreachable; not de-listing");
                    discovery.unreachable += 1;
                }
            }
        }
        Ok(discovery)
    }
}

#[cfg(unix)]
fn is_hard_connection_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

#[cfg(unix)]
async fn try_describe(
    endpoint: &ControlEndpoint,
) -> Result<Option<DiscoveredSession>, ControlError> {
    let mut client = match Client::connect(endpoint).await {
        Ok(client) => client,
        Err(ControlError::Io(err)) if is_hard_connection_error(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    match client.describe().await {
        Ok(info) => Ok(Some(DiscoveredSession {
            info,
            endpoint: endpoint.clone(),
        })),
        Err(ControlError::Io(err)) if is_hard_connection_error(&err) => Ok(None),
        Err(ControlError::Closed) => Ok(None),
        Err(err) => {
            tracing::debug!(?err, "control: endpoint failed to describe");
            Err(err)
        }
    }
}
