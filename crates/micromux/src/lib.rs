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

mod bounded_log;
mod config;
mod diagnostics;
mod env;
mod graph;
mod health_check;
mod scheduler;
mod service;

use color_eyre::eyre;
use tokio::sync::mpsc;

pub use tokio_util::sync::CancellationToken;

pub use bounded_log::{AsyncBoundedLog, BoundedLog};
pub use config::{ConfigError, ConfigFile, find_config_file, from_str};
pub use diagnostics::{Printer, ToDiagnostics};
pub use scheduler::{Command, Event, LogUpdateKind, OutputStream, ServiceID};

pub(crate) type ServiceMap = indexmap::IndexMap<ServiceID, service::Service>;

/// A simplified view of a service for presentation (e.g. in a UI).
#[derive(Debug, Clone)]
pub struct ServiceDescriptor {
    /// Unique identifier of the service.
    pub id: ServiceID,
    /// Human-readable name of the service.
    pub name: String,
    /// Parsed and validated open ports.
    pub open_ports: Vec<u16>,
    /// Whether this service has a healthcheck configured.
    pub healthcheck_configured: bool,
}

/// Main entry point to run a micromux session.
#[derive()]
pub struct Micromux {
    services: ServiceMap,
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

        Ok(Self { services })
    }

    /// Return a snapshot of services suitable for presentation.
    ///
    /// The returned descriptors intentionally omit internal details required only by the
    /// scheduler.
    #[must_use]
    pub fn services(&self) -> Vec<ServiceDescriptor> {
        self.services
            .iter()
            .map(|(service_id, service)| ServiceDescriptor {
                id: service_id.clone(),
                name: service.name.as_ref().clone(),
                open_ports: service.open_ports.clone(),
                healthcheck_configured: service.health_check.is_some(),
            })
            .collect()
    }

    /// Start the scheduler with default options.
    ///
    /// # Errors
    ///
    /// Returns an error if the scheduler fails to start or if any service fails during startup.
    pub async fn start(
        &self,
        ui_tx: mpsc::Sender<scheduler::Event>,
        commands_rx: mpsc::Receiver<scheduler::Command>,
        shutdown: CancellationToken,
    ) -> eyre::Result<()> {
        self.start_with_options(ui_tx, commands_rx, shutdown, true)
            .await
    }

    /// Start the scheduler with explicit options.
    ///
    /// # Errors
    ///
    /// Returns an error if the scheduler fails to start or if any service fails during startup.
    pub async fn start_with_options(
        &self,
        ui_tx: mpsc::Sender<scheduler::Event>,
        commands_rx: mpsc::Receiver<scheduler::Command>,
        shutdown: CancellationToken,
        interactive_logs: bool,
    ) -> eyre::Result<()> {
        tracing::info!("starting");
        let (events_tx, events_rx) = mpsc::channel(1024);

        tokio::spawn({
            let shutdown = shutdown.clone();
            async move {
                shutdown.cancelled().await;
                tracing::warn!("received shutdown signal");
            }
        });

        crate::scheduler::scheduler(
            &self.services,
            commands_rx,
            events_rx,
            events_tx,
            ui_tx,
            shutdown.clone(),
            interactive_logs,
        )
        .await?;
        tracing::info!("exiting");
        Ok(())
    }
}
