pub mod bounded_log;
pub mod config;
pub mod diagnostics;
pub mod env;
pub mod graph;
pub mod health_check;
pub mod scheduler;
pub mod service;

use crate::scheduler::ServiceID;
use color_eyre::eyre;
use service::Service;
use tokio::sync::mpsc;

pub use tokio_util::sync::CancellationToken;

pub type ServiceMap = indexmap::IndexMap<ServiceID, Service>;

#[derive()]
pub struct Micromux {
    pub config_file: config::ConfigFile<diagnostics::FileId>,
    pub services: ServiceMap,
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
                let service =
                    Service::new(name.as_ref().clone(), &config_dir, service_config.clone())?;
                Ok::<_, eyre::Report>((service_id, service))
            })
            .collect::<Result<ServiceMap, _>>()?;

        Ok(Self {
            config_file,
            services,
        })
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
