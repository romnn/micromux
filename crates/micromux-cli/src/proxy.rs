//! The shared proxy from the CLI's side: the `micromux proxy` subcommand, and how a session finds
//! the proxy, latches its port, and keeps its routes registered.

use micromux::{CancellationToken, Micromux, Session};

use crate::Error;

/// Keeps a session's routes registered for as long as it is held.
pub(crate) struct RouteRegistration {
    #[cfg(unix)]
    registrar: micromux_proxy::Registrar,
}

impl RouteRegistration {
    /// Reports the registration's status to the session's adapters, and keeps the registered
    /// routes in step with the session's route owners, until `shutdown` fires.
    #[cfg(unix)]
    pub(crate) fn report_to(&self, session: micromux::SessionStatus, shutdown: CancellationToken) {
        follow_route_owners(&session, self.registrar.routes(), shutdown.clone());

        let mut status = self.registrar.status();
        tokio::spawn(async move {
            loop {
                let routing = match &*status.borrow_and_update() {
                    micromux_proxy::RegistrationStatus::Registered => {
                        micromux::RoutingStatus::Registered
                    }
                    micromux_proxy::RegistrationStatus::Unavailable { reason } => {
                        micromux::RoutingStatus::Unavailable {
                            reason: reason.clone(),
                        }
                    }
                };
                session.set_routing(routing);
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    changed = status.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                }
            }
        });
    }

    /// Routes do not exist on platforms without a proxy.
    #[cfg(not(unix))]
    #[expect(
        clippy::unused_self,
        reason = "keeps one call shape with the Unix implementation"
    )]
    pub(crate) fn report_to(
        &self,
        _session: micromux::SessionStatus,
        _shutdown: CancellationToken,
    ) {
    }
}

/// Finds or starts the proxy for a session with routes and latches the port it listens on, which
/// the session's variables report.
///
/// # Errors
///
/// Returns an error when no acceptable proxy runs and none can be started.
#[cfg(unix)]
pub(crate) async fn latch_port(session: &mut Session) -> Result<(), Error> {
    if !session.has_routes() {
        return Ok(());
    }
    let (port, fallback_port) = session.proxy_preference();
    let port = micromux_proxy::ensure_proxy(
        &micromux_proxy::ProxyPaths::user()?,
        micromux_proxy::PortPreference {
            port,
            fallback_port,
        },
        Some(&launcher()?),
    )
    .await?;
    session.set_proxy_port(port);
    Ok(())
}

/// Routes are rejected on platforms without a proxy, so there is nothing to latch.
#[cfg(not(unix))]
#[expect(
    clippy::unused_async,
    reason = "keeps one call shape with the Unix implementation"
)]
pub(crate) async fn latch_port(_session: &mut Session) -> Result<(), Error> {
    Ok(())
}

/// Registers the session's routes before any service starts, and keeps them registered until
/// `shutdown` fires or the returned value is dropped.
///
/// # Errors
///
/// Returns an error when the proxy refuses the routes, such as when another live session already
/// serves the same session domain.
#[cfg(unix)]
pub(crate) async fn register(
    mux: &Micromux,
    config_path: &std::path::Path,
    shutdown: &CancellationToken,
) -> Result<Option<RouteRegistration>, Error> {
    use micromux_proxy::protocol::Registration;

    if mux.routes().is_empty() {
        return Ok(None);
    }
    let identity = mux.session_identity();
    let registration = Registration {
        session: identity.name.clone(),
        session_domain: identity.session_domain(),
        config_path: config_path.display().to_string(),
        pid: std::process::id(),
        proxy_port: mux.proxy_port(),
        routes: route_targets(mux.routes()),
    };
    let registrar = micromux_proxy::Registrar::start(
        micromux_proxy::ProxyPaths::user()?,
        registration,
        mux.proxy_port(),
        Some(launcher()?),
        shutdown,
    )
    .await?;
    Ok(Some(RouteRegistration { registrar }))
}

/// Forwards the session's routes to `routes` whenever they change, until `shutdown` fires.
///
/// A config reload can move a route to another service, which the proxy's error pages name.
#[cfg(unix)]
fn follow_route_owners(
    session: &micromux::SessionStatus,
    routes: tokio::sync::watch::Sender<Vec<micromux_proxy::protocol::RouteTarget>>,
    shutdown: CancellationToken,
) {
    let mut summary = session.subscribe();
    tokio::spawn(async move {
        loop {
            let targets = route_targets(&summary.borrow_and_update().routes);
            routes.send_if_modified(|current| {
                let changed = *current != targets;
                *current = targets;
                changed
            });
            tokio::select! {
                () = shutdown.cancelled() => return,
                changed = summary.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    });
}

/// The proxy's view of the session's routes.
#[cfg(unix)]
fn route_targets(routes: &[micromux::Route]) -> Vec<micromux_proxy::protocol::RouteTarget> {
    routes
        .iter()
        .map(|route| micromux_proxy::protocol::RouteTarget {
            host: route.host.clone(),
            port: route.port,
            service: route.service.clone(),
        })
        .collect()
}

/// Routes are rejected on platforms without a proxy, so there is nothing to register.
#[cfg(not(unix))]
#[expect(
    clippy::unused_async,
    reason = "keeps one call shape with the Unix implementation"
)]
pub(crate) async fn register(
    _mux: &Micromux,
    _config_path: &std::path::Path,
    _shutdown: &CancellationToken,
) -> Result<Option<RouteRegistration>, Error> {
    Ok(None)
}

/// Starts this same binary's `proxy` subcommand.
#[cfg(unix)]
fn launcher() -> Result<micromux_proxy::ProxyLauncher, Error> {
    Ok(micromux_proxy::ProxyLauncher::new(
        std::env::current_exe()?,
        ["proxy"],
    ))
}

/// Runs the proxy until it has been idle for `idle_timeout` or is told to stop.
///
/// Failures are printed to stderr, where a session that started this proxy reads them.
///
/// # Errors
///
/// Returns [`Error::DiagnosticsEmitted`] after printing why the proxy cannot run.
#[cfg(unix)]
pub(crate) async fn run(
    port: u16,
    fallback_port: Option<u16>,
    idle_timeout: std::time::Duration,
) -> Result<(), Error> {
    use std::io::IsTerminal as _;

    let shutdown = CancellationToken::new();
    spawn_signal_handler(shutdown.clone());
    let paths = match micromux_proxy::ProxyPaths::user() {
        Ok(paths) => paths,
        Err(err) => {
            eprintln!("error: cannot prepare the proxy's directory: {err}");
            return Err(Error::DiagnosticsEmitted);
        }
    };
    // A session that started this proxy stops reading its stderr once it is up, so only an
    // interactive run announces the port.
    let interactive = std::io::stderr().is_terminal();
    let result = micromux_proxy::serve(
        micromux_proxy::ServeOptions {
            paths,
            preference: micromux_proxy::PortPreference {
                port,
                fallback_port,
            },
            idle_timeout,
        },
        shutdown,
        |port| {
            if interactive {
                eprintln!("micromux proxy listening on port {port}");
            }
        },
    )
    .await;
    result.map_err(|err| {
        eprintln!("error: {err}");
        Error::DiagnosticsEmitted
    })
}

/// The proxy is Unix only.
#[cfg(not(unix))]
#[expect(
    clippy::unused_async,
    reason = "keeps one call shape with the Unix implementation"
)]
pub(crate) async fn run(
    _port: u16,
    _fallback_port: Option<u16>,
    _idle_timeout: std::time::Duration,
) -> Result<(), Error> {
    Err(Error::Message(
        "the micromux proxy is not supported on this platform".to_string(),
    ))
}

/// Stops the proxy on Ctrl-C or SIGTERM and ignores SIGHUP, so a closing terminal does not take
/// down routes that other sessions rely on.
#[cfg(unix)]
fn spawn_signal_handler(shutdown: CancellationToken) {
    use tokio::signal::unix::{SignalKind, signal};

    tokio::spawn(async move {
        let mut terminate = signal(SignalKind::terminate()).ok();
        let mut hangup = signal(SignalKind::hangup()).ok();
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                Some(()) = async { terminate.as_mut()?.recv().await } => break,
                Some(()) = async { hangup.as_mut()?.recv().await } => {}
            }
        }
        shutdown.cancel();
    });
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use color_eyre::eyre;
    use similar_asserts::assert_eq;

    const CONFIG: &str = r#"version: 1
routes:
  - host: app
    port: 41001
services:
  web:
    command: ["sh", "-c", "sleep 60"]
    ports: [41001]
"#;

    /// A reconcile that moves a route to another service re-registers it with the proxy under its
    /// new owner, which the 502 page then names.
    #[tokio::test]
    async fn reconciled_route_owners_reach_the_proxy_registration() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let config_path = directory.path().join("micromux.yaml");
        std::fs::write(&config_path, CONFIG)?;
        let mut config = micromux::from_str(CONFIG, directory.path(), 0, None, &mut Vec::new())?;
        config.config_path = Some(config_path.canonicalize()?);
        let registry = micromux::PortRegistry::at(directory.path().join("registry"));
        let mut session = micromux::Session::establish(&config, &registry)?;
        session.set_proxy_port(8080);
        let mux = Arc::new(micromux::Micromux::with_session(&config, session)?);
        let shutdown = micromux::CancellationToken::new();
        let (runner, handles) = mux.clone().start(shutdown.clone());
        let runner = tokio::spawn(runner);

        // Stand in for the registrar, starting from what the session registered at startup.
        let (routes, mut registered) =
            tokio::sync::watch::channel(super::route_targets(mux.routes()));
        super::follow_route_owners(&handles.session, routes, shutdown.clone());
        assert_eq!(
            registered
                .borrow_and_update()
                .first()
                .and_then(|route| route.service.clone()),
            Some("web".to_string())
        );

        // Move port 41001 from `web` to the new `frontend`.
        std::fs::write(
            &config_path,
            CONFIG.replace(
                "    ports: [41001]\n",
                "    ports: [41002]\n  frontend:\n    command: [\"sh\", \"-c\", \"sleep 60\"]\n    ports: [41001]\n",
            ),
        )?;
        handles
            .service_control()
            .reconcile_config(false)
            .await
            .map_err(|_| eyre::eyre!("scheduler stopped"))?
            .map_err(|rejection| eyre::eyre!("reconcile rejected: {rejection}"))?;

        // The registration follows with the new owner.
        tokio::time::timeout(
            Duration::from_secs(10),
            registered.wait_for(|routes| {
                routes.first().and_then(|route| route.service.as_deref()) == Some("frontend")
            }),
        )
        .await??;

        shutdown.cancel();
        runner.await??;
        Ok(())
    }
}
