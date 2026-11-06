//! The shared per-user HTTP proxy that serves every session's routes under
//! `http://<route>.<session>.<domain>`.
//!
//! One proxy runs per user.
//! It listens on loopback, routes each request by its `Host` header to the `127.0.0.1` port a
//! session registered for it, and passes WebSocket upgrades and streaming responses through
//! without buffering.
//! Sessions register over a local Unix socket for as long as they hold the connection open (see
//! [`protocol`]), and the first session that needs the proxy starts it.
//! The proxy exits by itself once no session has been registered for its idle period.
//!
//! Unix only: on other platforms only the [`protocol`] types exist.

pub mod protocol;

#[cfg(unix)]
mod client;
#[cfg(unix)]
mod forward;
#[cfg(unix)]
mod listen;
#[cfg(unix)]
mod pages;
#[cfg(unix)]
mod paths;
#[cfg(unix)]
mod server;

#[cfg(unix)]
pub use client::{ClientError, ProxyLauncher, Registrar, RegistrationStatus, ensure_proxy};
#[cfg(unix)]
pub use listen::{BindError, PortPreference};
#[cfg(unix)]
pub use paths::ProxyPaths;
#[cfg(unix)]
pub use server::{ServeError, ServeOptions, serve};
