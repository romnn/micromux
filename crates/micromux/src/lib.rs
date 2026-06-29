//! `micromux` is a small process supervisor with a terminal UI.
//!
//! The crate provides the core scheduling and service-management logic used by the CLI and TUI.
//! Most users will interact with it through [`Micromux`] and configuration parsing via
//! [`from_str`].
//!
//! # Examples
//!
//! Parse a configuration file and construct a [`Micromux`] instance:
//!
//! ```no_run
//! # use color_eyre::eyre;
//! # fn main() -> eyre::Result<()> {
//! let raw = std::fs::read_to_string("./micromux.yaml")?;
//! let config_dir = std::path::Path::new(".");
//! let file_id = 0usize;
//! let mut diagnostics = vec![];
//! let config = micromux::from_str(&raw, config_dir, file_id, None, &mut diagnostics)?;
//! let mux = micromux::Micromux::new(&config)?;
//! # Ok(()) }
//! ```

mod config;
mod diagnostics;
mod env;
mod graph;
mod health_check;
mod model;
mod scheduler;
mod service;

use color_eyre::eyre;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;

pub use tokio_util::sync::CancellationToken;

pub use config::{ConfigError, ConfigFile, config_file_names, find_config_file, from_str};
pub use diagnostics::{Printer, ToDiagnostics};
pub use health_check::Health;
pub use model::{
    ChangeKind, Desired, DiskLogRetention, Execution, HealthAttempt, HealthLine, HealthResult,
    LogLimit, LogLine, LogRetention, LogRun, LogRunSummary, MemoryLogRetention, ServiceSnapshot,
    SessionChange, SessionModelReader,
};
pub use scheduler::{
    Command, CommandRejection, OutputStream, SchedulerStopped, ServiceCommandAck,
    ServiceCommandResult, ServiceControl, ServiceID,
};
pub use service::RestartPolicy;

pub(crate) type ServiceMap = indexmap::IndexMap<ServiceID, service::Service>;

/// Main entry point to run a micromux session.
pub struct Micromux {
    services: ServiceMap,
}

/// Capability handles returned by [`Micromux::start`].
///
/// The model writer never escapes the core, so the only handles an adapter can hold are the read
/// capability and a command sender. The narrow [`ServiceControl`] port (no input forwarding) is
/// derived from [`Handles::service_control`] for untrusted adapters; the trusted in-process TUI
/// keeps the full [`Handles::commands`] sender.
pub struct Handles {
    /// Read capability over the session model: query + `subscribe`.
    pub reader: SessionModelReader,
    /// Full trusted in-process command sender for the TUI/CLI.
    pub commands: mpsc::Sender<Command>,
}

impl Handles {
    /// A narrow, untrusted command port (restart/enable/disable only) for adapters such as the
    /// control server and MCP. It cannot express `SendInput`/`ResizeAll`.
    #[must_use]
    pub fn service_control(&self) -> ServiceControl {
        ServiceControl::new(self.commands.clone())
    }
}

/// Return the OS-specific project directories for micromux.
///
/// This can be used by frontends (CLI/TUI) to determine where to store log files and other
/// persistent state.
#[must_use]
pub fn project_dir() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("com", "romnn", "micromux")
}

impl Micromux {
    /// Construct a new [`Micromux`] instance from a parsed [`ConfigFile`].
    ///
    /// # Errors
    ///
    /// Returns an error if a service definition in the configuration cannot be normalized
    /// (e.g. invalid environment interpolation, invalid port parsing, etc.).
    pub fn new(config_file: &config::ConfigFile<diagnostics::FileId>) -> eyre::Result<Self> {
        let config_dir = config_file.config_dir.clone();
        let services = config_file
            .config
            .services
            .iter()
            .map(|(name, service_config)| {
                let service_id = name.as_ref().clone();
                let service = service::Service::new(
                    name.as_ref().clone(),
                    &config_dir,
                    service_config.clone(),
                )?;
                Ok::<_, eyre::Report>((service_id, service))
            })
            .collect::<Result<ServiceMap, _>>()?;

        graph::ServiceGraph::new(&services)?;

        Ok(Self { services })
    }

    /// Return a snapshot of services suitable for presentation.
    ///
    /// The returned descriptors intentionally omit internal details required only by the
    /// scheduler.
    fn initial_model_entries(&self) -> Vec<(ServiceSnapshot, LogRetention)> {
        self.services
            .iter()
            .map(|(id, service)| {
                (
                    ServiceSnapshot::initial(
                        id.clone(),
                        service.name.as_ref().clone(),
                        service.open_ports.clone(),
                        service.health_check.is_some(),
                        service.restart_policy.clone(),
                    ),
                    service.log_retention,
                )
            })
            .collect()
    }

    /// Start the scheduler, returning the runner future and the capability [`Handles`].
    ///
    /// The model (`Inner` + `Writer`) and the command channel are built internally; the writer is
    /// moved into the runner future and never leaves the core, so adapters can only read the model
    /// or send commands. `Arc<Self>` makes the future `'static`, so the caller can `tokio::spawn` it
    /// while holding the handles.
    pub fn start(
        self: Arc<Self>,
        shutdown: CancellationToken,
    ) -> (impl Future<Output = eyre::Result<()>> + 'static, Handles) {
        let (reader, writer) = model::new(self.initial_model_entries());
        let (commands_tx, commands_rx) = mpsc::channel(1024);
        let handles = Handles {
            reader,
            commands: commands_tx,
        };

        let runner = async move {
            tracing::info!("starting");
            let (events_tx, events_rx) = mpsc::channel(1024);

            tokio::spawn({
                let shutdown = shutdown.clone();
                async move {
                    shutdown.cancelled().await;
                    tracing::warn!("received shutdown signal");
                }
            });

            scheduler::scheduler(
                &self.services,
                commands_rx,
                events_rx,
                events_tx,
                #[cfg(test)]
                None,
                writer,
                shutdown.clone(),
            )
            .await?;
            tracing::info!("exiting");
            Ok(())
        };

        (runner, handles)
    }
}
