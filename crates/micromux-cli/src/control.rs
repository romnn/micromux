//! The CLI-side control adapter: bind the per-session endpoint and run the control server.
//!
//! This is the untrusted boundary.
//! The server is constructed with two model capabilities — a `SessionModelReader` (queries +
//! subscribe) and a narrow `ServiceControl` (mutations and the environment read) — so it can
//! observe and command but can neither mutate the model directly nor forward raw input.
//! The accept loop additionally holds the session's shutdown token, its one lifecycle
//! capability: a `Request::Shutdown` cancels it to stop the whole session (the same path as
//! Ctrl-C).

use std::path::Path;
use std::sync::Arc;

use micromux::{CancellationToken, Handles};
use micromux_control::{
    CanonicalConfigPath, ControlServer, ProjectClaim, SessionIdentity, bind_project, claim_project,
    endpoint_for, runtime_dir,
};

/// Outcome of attempting to start the local control plane.
pub enum SpawnStatus {
    /// The endpoint is owned and its accept loop is running.
    Started,
    /// Another supervisor owns the same canonical config.
    OwnedByOther,
    /// The platform, runtime directory, or endpoint setup prevented control startup.
    Unavailable,
}

/// Resolve a human-readable session name: the config `name:` if set, else `basename(working_dir)`.
fn session_name(configured: Option<String>, working_dir: &Path) -> String {
    configured.unwrap_or_else(|| {
        working_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "micromux".to_string())
    })
}

/// Keeps the config's owner lock until `shutdown` fires, for a session without a control plane.
///
/// A session that stops releases its claim right away, like the endpoint it would serve, so a
/// session started meanwhile can claim the config and wait for the stopping one's cleanups.
pub fn release_at_shutdown(claim: Option<ProjectClaim>, shutdown: CancellationToken) {
    if let Some(claim) = claim {
        tokio::spawn(async move {
            shutdown.cancelled().await;
            drop(claim);
        });
    }
}

/// Bind the control endpoint (race-safe lifetime-lock dance) and spawn the accept loop.
///
/// Control is *default on*. The returned status distinguishes another owner from transport/setup
/// failure so the TUI can show an accurate warning; headless `serve` rejects either failure because
/// an unreachable session would be useless.
///
/// The endpoint is bound under `claim`, the config's owner lock the session took when it started,
/// or under a claim taken now when it has none.
/// The session keeps its claim even when the control plane is unavailable, and releases it when
/// `shutdown` fires.
pub fn spawn(
    handles: &Handles,
    claim: Option<ProjectClaim>,
    config_path: &CanonicalConfigPath,
    working_dir: &Path,
    name: Option<String>,
    shutdown: CancellationToken,
) -> SpawnStatus {
    if !micromux_control::transport_supported() {
        tracing::warn!("control plane disabled: unsupported platform");
        release_at_shutdown(claim, shutdown);
        return SpawnStatus::Unavailable;
    }

    let Some(runtime_dir) = runtime_dir() else {
        tracing::warn!("control plane disabled: no runtime directory could be resolved");
        release_at_shutdown(claim, shutdown);
        return SpawnStatus::Unavailable;
    };

    let claim = match claim.map_or_else(|| claim_project(config_path), Ok) {
        Ok(claim) => claim,
        Err(micromux_control::ClaimError::Held { .. }) => {
            tracing::warn!("control plane disabled: another micromux already owns this project");
            return SpawnStatus::OwnedByOther;
        }
        Err(micromux_control::ClaimError::Control(err)) => {
            tracing::warn!(?err, "control plane disabled");
            return SpawnStatus::Unavailable;
        }
    };
    let endpoint = endpoint_for(&runtime_dir, config_path);
    let bound = bind_project(&endpoint, &claim);
    release_at_shutdown(Some(claim), shutdown.clone());
    let guard = match bound {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            tracing::warn!(
                "control plane disabled: another micromux already owns this project's endpoint"
            );
            return SpawnStatus::OwnedByOther;
        }
        Err(err) => {
            tracing::warn!(?err, "control plane disabled");
            return SpawnStatus::Unavailable;
        }
    };

    let identity = SessionIdentity::new(session_name(name, working_dir), working_dir, config_path);
    let server = Arc::new(
        ControlServer::new(
            handles.reader.clone(),
            handles.service_control(),
            identity,
            handles.dynamic_services.clone(),
        )
        .with_session_status(handles.session.clone()),
    );
    tracing::info!(endpoint = ?endpoint, "control plane listening");

    tokio::spawn(async move {
        if let Err(err) = server.serve(guard, shutdown).await {
            tracing::warn!(?err, "control server exited with an error");
        }
    });
    SpawnStatus::Started
}

/// Resolve the canonical config path the same way `load_config` does, for endpoint derivation.
///
/// # Errors
///
/// Returns an error if no config file can be found or canonicalized.
pub async fn resolve_config_path(
    explicit: Option<&Path>,
    working_dir: &Path,
) -> Result<CanonicalConfigPath, crate::Error> {
    let config_path = match explicit {
        Some(path) => Some(path.to_path_buf()),
        None => micromux::find_config_file(working_dir).await?,
    };
    let config_path =
        config_path.ok_or_else(|| crate::Error::Message("missing config file".to_string()))?;
    Ok(CanonicalConfigPath::new(config_path)?)
}

#[cfg(all(test, unix))]
mod tests {
    use color_eyre::eyre;

    /// A session's claim on its config lasts until the session starts stopping, so a session
    /// started meanwhile can claim the config and wait for the stopping one's cleanups.
    #[tokio::test]
    async fn a_claim_is_released_when_the_session_starts_stopping() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        // A lock of the test's own, in place of the config's in the user's coordination
        // directory.
        let lock = directory.path().join("project.session.lock");
        let shutdown = micromux::CancellationToken::new();

        super::release_at_shutdown(
            Some(micromux_control::claim_project_at(&lock)?),
            shutdown.clone(),
        );
        tokio::task::yield_now().await;
        eyre::ensure!(
            matches!(
                micromux_control::claim_project_at(&lock),
                Err(micromux_control::ClaimError::Held { .. })
            ),
            "the claim was released while the session ran"
        );

        shutdown.cancel();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while micromux_control::claim_project_at(&lock).is_err() {
            eyre::ensure!(
                tokio::time::Instant::now() < deadline,
                "the claim outlived the shutdown"
            );
            tokio::task::yield_now().await;
        }
        Ok(())
    }
}
