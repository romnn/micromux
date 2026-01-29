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

#[derive(Debug, Clone)]
pub struct ServiceDescriptor {
    pub id: ServiceID,
    pub name: String,
    pub open_ports: Vec<u16>,
    pub healthcheck_configured: bool,
}

#[derive()]
pub struct Micromux {
    config_file: config::ConfigFile<diagnostics::FileId>,
    services: ServiceMap,
}

pub fn project_dir() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("com", "romnn", "micromux")
}

impl Micromux {
    pub fn new(config_file: config::ConfigFile<diagnostics::FileId>) -> eyre::Result<Self> {
        let config_dir = config_file.config_dir.clone();
        let services = config_file
            .config
            .services
            .iter()
            .map(|(name, service_config)| {
                let service_id = name.as_ref().to_string();
                let service = service::Service::new(
                    name.as_ref().clone(),
                    &config_dir,
                    service_config.clone(),
                )?;
                Ok::<_, eyre::Report>((service_id, service))
            })
            .collect::<Result<ServiceMap, _>>()?;

        Ok(Self {
            config_file,
            services,
        })
    }

    pub fn services(&self) -> Vec<ServiceDescriptor> {
        self.services
            .iter()
            .map(|(service_id, service)| ServiceDescriptor {
                id: service_id.clone(),
                name: service.name.as_ref().to_string(),
                open_ports: service.open_ports.clone(),
                healthcheck_configured: service.health_check.is_some(),
            })
            .collect()
    }

    pub async fn start(
        &self,
        ui_tx: mpsc::Sender<scheduler::Event>,
        commands_rx: mpsc::Receiver<scheduler::Command>,
        shutdown: CancellationToken,
    ) -> eyre::Result<()> {
        self.start_with_options(ui_tx, commands_rx, shutdown, true)
            .await
    }

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
