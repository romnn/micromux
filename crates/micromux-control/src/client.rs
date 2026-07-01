//! The control client used by `micromux ctl` and the MCP proxy.
//!
//! Endpoint probing skips sockets that refuse a connection (dead), surfaces busy/incompatible
//! endpoints, and never prunes them.

use std::path::{Path, PathBuf};

use futures::future;

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

/// Probe result for one concrete endpoint.
#[derive(Debug, Clone)]
pub struct EndpointProbe {
    /// The endpoint that was probed.
    pub endpoint: ControlEndpoint,
    /// What happened when it was probed.
    pub result: EndpointProbeResult,
}

/// The outcome of probing one endpoint.
#[derive(Debug, Clone)]
pub enum EndpointProbeResult {
    /// The endpoint answered `Describe`.
    Session(Box<SessionInfo>),
    /// Nothing live answered on this endpoint.
    Absent(String),
    /// Something was listening, but it was busy or unusable.
    Unreachable(String),
}

/// Return every probe that answered `Describe`, preserving probe order.
#[must_use]
pub fn answering_session_probes(probes: &[EndpointProbe]) -> Vec<(ControlEndpoint, SessionInfo)> {
    probes
        .iter()
        .filter_map(|probe| match &probe.result {
            EndpointProbeResult::Session(info) => Some((probe.endpoint.clone(), (**info).clone())),
            EndpointProbeResult::Absent(_) | EndpointProbeResult::Unreachable(_) => None,
        })
        .collect()
}

/// Return answering sessions deduped by the session start token, preserving first-seen order.
#[must_use]
pub fn unique_answering_session_probes(
    probes: &[EndpointProbe],
) -> Vec<(ControlEndpoint, SessionInfo)> {
    let mut sessions = Vec::<(ControlEndpoint, SessionInfo)>::new();
    for (endpoint, info) in answering_session_probes(probes) {
        if !sessions
            .iter()
            .any(|(_, existing)| existing.is_same_instance(&info))
        {
            sessions.push((endpoint, info));
        }
    }
    sessions
}

impl Client {
    /// Connect to a control endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError::Io`] if the endpoint refuses the connection (dead session) or
    /// [`ControlError::Unsupported`] on a platform without a transport.
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unused_async,
            reason = "Unix has the async transport implementation; unsupported platforms keep the same API shape"
        )
    )]
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
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unused_async,
            reason = "Unix has the async transport implementation; unsupported platforms keep the same API shape"
        )
    )]
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
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unused_async,
            reason = "Unix has the async transport implementation; unsupported platforms keep the same API shape"
        )
    )]
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
    #[cfg_attr(
        not(unix),
        expect(
            clippy::unused_async,
            reason = "Unix reads from an async subscription stream; unsupported platforms keep the same API shape"
        )
    )]
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

/// Probe every socket endpoint in one runtime directory.
///
/// Hard-dead endpoints are included as [`EndpointProbeResult::Absent`] instead of being dropped, so
/// callers can surface detailed discovery diagnostics.
///
/// # Errors
///
/// Returns [`ControlError::Unsupported`] on platforms without a concrete control transport.
pub async fn probe_runtime_dir(runtime_dir: &Path) -> Result<Vec<EndpointProbe>, ControlError> {
    #[cfg(unix)]
    {
        let endpoints = runtime_dir_endpoints(runtime_dir);
        Ok(probe_endpoints(&endpoints).await)
    }

    #[cfg(not(unix))]
    {
        let _ = runtime_dir;
        Err(ControlError::Unsupported)
    }
}

/// Probe every socket endpoint across runtime directories in one concurrent batch.
///
/// # Errors
///
/// Returns [`ControlError::Unsupported`] on platforms without a concrete control transport.
pub async fn probe_runtime_dirs(
    runtime_dirs: &[PathBuf],
) -> Result<Vec<EndpointProbe>, ControlError> {
    #[cfg(unix)]
    {
        let mut endpoints = Vec::new();
        for runtime_dir in runtime_dirs {
            endpoints.extend(runtime_dir_endpoints(runtime_dir));
        }
        Ok(probe_endpoints(&endpoints).await)
    }

    #[cfg(not(unix))]
    {
        let _ = runtime_dirs;
        Err(ControlError::Unsupported)
    }
}

/// Probe known endpoints in order.
pub async fn probe_endpoints(endpoints: &[ControlEndpoint]) -> Vec<EndpointProbe> {
    future::join_all(endpoints.iter().map(probe_endpoint)).await
}

/// Probe one known endpoint.
pub async fn probe_endpoint(endpoint: &ControlEndpoint) -> EndpointProbe {
    #[cfg(unix)]
    {
        let mut client = match Client::connect(endpoint).await {
            Ok(client) => client,
            Err(ControlError::Io(err)) if is_hard_connection_error(&err) => {
                return EndpointProbe {
                    endpoint: endpoint.clone(),
                    result: EndpointProbeResult::Absent(format!("connect: {err}")),
                };
            }
            Err(err) => {
                return EndpointProbe {
                    endpoint: endpoint.clone(),
                    result: EndpointProbeResult::Unreachable(err.to_string()),
                };
            }
        };

        match client.describe().await {
            Ok(info) => EndpointProbe {
                endpoint: endpoint.clone(),
                result: EndpointProbeResult::Session(Box::new(info)),
            },
            Err(ControlError::Io(err)) if is_hard_connection_error(&err) => EndpointProbe {
                endpoint: endpoint.clone(),
                result: EndpointProbeResult::Absent(format!("describe: {err}")),
            },
            Err(ControlError::Closed) => EndpointProbe {
                endpoint: endpoint.clone(),
                result: EndpointProbeResult::Unreachable("closed before Describe".to_string()),
            },
            Err(err) => EndpointProbe {
                endpoint: endpoint.clone(),
                result: EndpointProbeResult::Unreachable(err.to_string()),
            },
        }
    }

    #[cfg(not(unix))]
    {
        EndpointProbe {
            endpoint: endpoint.clone(),
            result: EndpointProbeResult::Unreachable(
                "the control plane is not supported on this platform".to_string(),
            ),
        }
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
fn runtime_dir_endpoints(runtime_dir: &Path) -> Vec<ControlEndpoint> {
    let Ok(entries) = std::fs::read_dir(runtime_dir) else {
        return Vec::new();
    };
    let mut endpoints = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("sock") {
            continue;
        }
        endpoints.push(ControlEndpoint::Unix(path));
    }
    endpoints.sort();
    endpoints
}

#[cfg(test)]
mod tests {
    use super::*;
    use similar_asserts::assert_eq;
    use std::path::PathBuf;

    fn session(id: &str, pid: u32, start_time: u64, name: &str) -> SessionInfo {
        SessionInfo {
            protocol_version: PROTOCOL_VERSION,
            id: id.to_string(),
            pid,
            start_time,
            name: name.to_string(),
            working_dir: ".".to_string(),
            config_path: "/project/micromux.yaml".to_string(),
            services: Vec::new(),
            micromux_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    #[test]
    fn unique_answering_session_probes_dedupes_aliases() {
        let first = session("aaa", 42, 100, "first");
        let alias = session("aaa", 42, 100, "alias");
        let second = session("bbb", 43, 101, "second");
        let probes = vec![
            EndpointProbe {
                endpoint: ControlEndpoint::Unix(PathBuf::from("/first.sock")),
                result: EndpointProbeResult::Session(Box::new(first)),
            },
            EndpointProbe {
                endpoint: ControlEndpoint::Unix(PathBuf::from("/alias.sock")),
                result: EndpointProbeResult::Session(Box::new(alias)),
            },
            EndpointProbe {
                endpoint: ControlEndpoint::Unix(PathBuf::from("/second.sock")),
                result: EndpointProbeResult::Session(Box::new(second)),
            },
        ];

        let sessions = unique_answering_session_probes(&probes);

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].1.name, "first");
        assert_eq!(sessions[1].1.name, "second");
    }

    #[cfg(unix)]
    #[test]
    fn runtime_dir_endpoints_are_sorted() -> color_eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("z.sock"), b"")?;
        std::fs::write(dir.path().join("a.sock"), b"")?;
        std::fs::write(dir.path().join("ignore.txt"), b"")?;

        let endpoints = runtime_dir_endpoints(dir.path());

        assert_eq!(
            endpoints,
            vec![
                ControlEndpoint::Unix(dir.path().join("a.sock")),
                ControlEndpoint::Unix(dir.path().join("z.sock")),
            ],
        );
        Ok(())
    }
}
