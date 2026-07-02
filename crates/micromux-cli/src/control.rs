//! The CLI-side control adapter: bind the per-session endpoint and run the control server.
//!
//! This is the untrusted boundary. The server is constructed with two model capabilities — a
//! `SessionModelReader` (queries + subscribe) and a narrow `ServiceControl` (mutations) — so it can
//! observe and command but can neither mutate the model directly nor forward raw input. The accept
//! loop additionally holds the session's shutdown token, its one lifecycle capability: a
//! `Request::Shutdown` cancels it to stop the whole session (the same path as Ctrl-C).

use std::path::Path;
use std::sync::Arc;

use color_eyre::eyre;
use micromux::{CancellationToken, Handles};
use micromux_control::{ControlServer, SessionIdentity, bind, endpoint_for, runtime_dir};

/// Resolve a human-readable session name: the config `name:` if set, else `basename(working_dir)`.
fn session_name(configured: Option<String>, working_dir: &Path) -> String {
    configured.unwrap_or_else(|| {
        working_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "micromux".to_string())
    })
}

/// Bind the control endpoint (race-safe lifetime-lock dance) and spawn the accept loop.
///
/// Control is *default on*; this is a no-op (returns `false`) when the runtime dir is unresolvable,
/// the transport is unsupported on this platform, or another live session already owns the project's
/// endpoint. In the TUI that is harmless — the binary is never half-working. The headless `serve`
/// mode uses the return value to exit when it cannot own the endpoint (an unreachable headless
/// session would be useless).
pub fn spawn(
    handles: &Handles,
    config_path: &Path,
    working_dir: &Path,
    name: Option<String>,
    shutdown: CancellationToken,
) -> bool {
    if !micromux_control::transport_supported() {
        tracing::warn!("control plane disabled: unsupported platform");
        return false;
    }

    let Some(runtime_dir) = runtime_dir() else {
        tracing::warn!("control plane disabled: no runtime directory could be resolved");
        return false;
    };

    let endpoint = endpoint_for(&runtime_dir, config_path);
    let guard = match bind(&endpoint) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            tracing::warn!(
                "control plane disabled: another micromux already owns this project's endpoint"
            );
            return false;
        }
        Err(err) => {
            tracing::warn!(?err, "control plane disabled");
            return false;
        }
    };

    let identity = SessionIdentity::new(session_name(name, working_dir), working_dir, config_path);
    let server = Arc::new(ControlServer::new(
        handles.reader.clone(),
        handles.service_control(),
        identity,
    ));
    tracing::info!(endpoint = ?endpoint, "control plane listening");

    tokio::spawn(async move {
        if let Err(err) = server.serve(guard, shutdown).await {
            tracing::warn!(?err, "control server exited with an error");
        }
    });
    true
}

/// Resolve the canonical config path the same way `load_config` does, for endpoint derivation.
///
/// # Errors
///
/// Returns an error if no config file can be found or canonicalized.
pub async fn resolve_config_path(
    explicit: Option<&Path>,
    working_dir: &Path,
) -> eyre::Result<std::path::PathBuf> {
    let config_path = match explicit {
        Some(path) => Some(path.to_path_buf()),
        None => micromux::find_config_file(working_dir).await?,
    };
    let config_path = config_path.ok_or_else(|| eyre::eyre!("missing config file"))?;
    Ok(config_path.canonicalize()?)
}
