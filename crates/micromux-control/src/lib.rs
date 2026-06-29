//! The micromux control plane: a per-session local control endpoint and its wire protocol.
//!
//! The session binds a Unix domain socket (Unix/macOS) keyed by a hash of its canonical config
//! path; clients — the `micromux ctl` CLI and the MCP proxy — connect, speak newline-delimited JSON
//! ([`Request`]/[`Response`]), and never touch the filesystem beyond connecting. The core knows
//! nothing about any of this; the server is driven by a [`micromux::SessionModelReader`] (reads) and
//! a [`micromux::ServiceControl`] (mutations), so it can observe and command but never mutate the
//! model directly.

mod client;
mod endpoint;
mod protocol;
mod server;

use std::time::Duration;

#[cfg(unix)]
use futures::{SinkExt, StreamExt};
#[cfg(unix)]
use serde::Serialize;
#[cfg(unix)]
use serde::de::DeserializeOwned;
#[cfg(unix)]
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::LinesCodecError;
#[cfg(unix)]
use tokio_util::codec::{Framed, LinesCodec};

pub use client::{Client, DiscoveredSession, Discovery, Subscription, discover_sessions};
pub use endpoint::{
    ControlEndpoint, endpoint_for, endpoint_from_hash, endpoint_hash, runtime_dir,
    transport_supported,
};
pub use protocol::{ErrorCode, PROTOCOL_VERSION, Request, Response, ServiceBrief, SessionInfo};
pub use server::{ControlServer, EndpointGuard, SessionIdentity, bind};

/// Maximum size of a single protocol frame. Oversized frames are rejected, not buffered, so a
/// broken peer cannot pin memory.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// How long a client waits for a single response before giving up.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a server connection may sit idle (no request) before it is closed.
pub const IDLE_TIMEOUT: Duration = Duration::from_mins(5);

/// Errors produced by the control plane.
#[derive(thiserror::Error, Debug)]
pub enum ControlError {
    /// An I/O error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A JSON (de)serialization error.
    #[error("control protocol serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    /// The control plane is not supported on this platform.
    #[error("the control plane is not supported on this platform")]
    Unsupported,
    /// The connection was closed before a response arrived.
    #[error("control connection closed")]
    Closed,
    /// A frame exceeded [`MAX_FRAME_BYTES`].
    #[error("control protocol frame exceeded the maximum size")]
    FrameTooLarge,
    /// A request timed out.
    #[error("control request timed out")]
    Timeout,
    /// The peer speaks an incompatible protocol version.
    #[error("control protocol version mismatch: peer={peer}, ours={ours}")]
    ProtocolMismatch {
        /// The peer's protocol version.
        peer: u32,
        /// Our protocol version.
        ours: u32,
    },
    /// The peer sent a response that did not match the request.
    #[error("unexpected control response: {0}")]
    Unexpected(String),
}

impl From<LinesCodecError> for ControlError {
    fn from(err: LinesCodecError) -> Self {
        match err {
            LinesCodecError::MaxLineLengthExceeded => Self::FrameTooLarge,
            LinesCodecError::Io(err) => Self::Io(err),
        }
    }
}

#[cfg(unix)]
type Framing<T> = Framed<T, LinesCodec>;

#[cfg(unix)]
fn framed<T: AsyncRead + AsyncWrite>(io: T) -> Framing<T> {
    Framed::new(io, LinesCodec::new_with_max_length(MAX_FRAME_BYTES))
}

#[cfg(unix)]
async fn write_message<T, M>(framing: &mut Framing<T>, message: &M) -> Result<(), ControlError>
where
    T: AsyncRead + AsyncWrite + Unpin,
    M: Serialize,
{
    let line = serde_json::to_string(message)?;
    framing.send(line).await?;
    Ok(())
}

#[cfg(unix)]
async fn read_message<T, M>(framing: &mut Framing<T>) -> Result<Option<M>, ControlError>
where
    T: AsyncRead + AsyncWrite + Unpin,
    M: DeserializeOwned,
{
    match framing.next().await {
        Some(Ok(line)) => Ok(Some(serde_json::from_str(&line)?)),
        Some(Err(err)) => Err(err.into()),
        None => Ok(None),
    }
}
