#![allow(warnings)]
#![deny(unused_must_use)]

pub mod bounded_log;
pub mod config;
pub mod diagnostics;
pub mod env;
pub mod graph;
pub mod health_check;
pub mod scheduler;
pub mod service;

use crate::scheduler::{Command, ServiceID};
use color_eyre::eyre;
use service::{RestartPolicy, Service};
use std::collections::HashMap;
use tokio::sync::{Notify, mpsc};
use yaml_spanned::Spanned;

pub use tokio_util::sync::CancellationToken;

pub type ServiceMap = indexmap::IndexMap<ServiceID, Service>;

#[derive()]
pub struct Micromux {
    pub config_file: config::ConfigFile<diagnostics::FileId>,
    pub services: ServiceMap,
    // pub services: HashMap<Spanned<String>, Service>,
    // pub graph: petgraph::Graph<String, ()>,
    // pub project_dir: directories::ProjectDirs,
    // state_change: Notify,
    // pub shutdown: CancellationToken,
    // pub shutdown: Shutdown,
    // app: micromux_tui::App,
}

pub fn project_dir() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("com", "romnn", "micromux")
}

impl Micromux {
    pub fn new(
        config_file: config::ConfigFile<diagnostics::FileId>,
        // shutdown: Shutdown,
    ) -> eyre::Result<Self> {
        let config_dir = config_file.config_dir.clone();
        let services = config_file
            .config
            .services
            .iter()
            .map(|(name, service_config)| {
                let service_id = name.as_ref().to_string();
                let service = Service::new(name.as_ref().clone(), &config_dir, service_config.clone())?;
                Ok::<_, eyre::Report>((service_id, service))
            })
            .collect::<Result<ServiceMap, _>>()?;

        // build graph
        // let graph = graph::ServiceGraph::new(&config_file.config)?;

        // let shutdown = CancellationToken::new();

        Ok(Self {
            config_file,
            services,
            // graph: graph.inner,
            // state_change: Notify::new(),
            // shutdown,
            // shutdown,
            // project_dir,
        })
    }

    pub async fn start(
        &self,
        ui_tx: mpsc::Sender<scheduler::Event>,
        commands_rx: mpsc::Receiver<scheduler::Command>,
        shutdown: CancellationToken,
    ) -> eyre::Result<()> {
        // mpsc::Sender<Command>
        tracing::info!("starting");
        let (events_tx, events_rx) = mpsc::channel(1024);

        // TODO: change this to be a mpsc too
        // let (broadcast_tx, broadcast_rx) = tokio::sync::broadcast::channel(1024);
        // let (ui_tx, ui_rx) = mpsc::channel(1024);

        tokio::spawn({
            let mut shutdown = shutdown.clone();
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
        )
        .await?;
        tracing::info!("exiting");
        Ok(())
    }

    // pub fn disable_service(&self, service_id: &ServiceID) {
    //     self.commands_tx.send();
    // }
    //
    // pub fn restart_service(&self, service_id: &ServiceID) {
    //     // TODO
    // }

    // pub async fn start(&mut self) {
    //     let mut shutdown_handle = self.shutdown.handle();
    //     tracing::trace!("start");
    //     loop {
    //         let sources = self.graph.externals(petgraph::Direction::Incoming);
    //         dbg!(&sources.clone().collect::<Vec<_>>());
    //
    //         for source in sources {
    //             let mut bfs = petgraph::visit::Bfs::new(&self.graph, source);
    //             while let Some(nx) = bfs.next(&self.graph) {
    //                 tracing::trace!("visited node {}", self.graph[nx]);
    //
    //                 let node = &self.graph[nx];
    //                 let service = self.services.get_mut(node).unwrap();
    //
    //                 tracing::debug!(name = node, state = ?service.state, "evaluating service");
    //
    //                 // check if service should be (re)started
    //                 match service.state {
    //                     State::Pending => {}
    //                     State::Running | State::Disabled => continue,
    //                     State::Exited => match service.restart_policy {
    //                         RestartPolicy::Never => continue,
    //                         RestartPolicy::OnFailure { remaining_attempts }
    //                             if remaining_attempts <= 0 =>
    //                         {
    //                             continue;
    //                         }
    //                         RestartPolicy::OnFailure {
    //                             ref mut remaining_attempts,
    //                         } => {
    //                             *remaining_attempts = remaining_attempts.saturating_sub(1);
    //                         }
    //                         RestartPolicy::UnlessStopped | RestartPolicy::Always => {}
    //                     },
    //                 }
    //
    //                 // check if all dependencies are healthy
    //                 let dependencies = self.graph.edges_directed(nx, petgraph::Direction::Incoming);
    //                 let dependencies: Vec<&Service> = dependencies
    //                     .filter_map(|edge| {
    //                         use petgraph::visit::EdgeRef;
    //                         debug_assert_eq!(nx, edge.target());
    //                         let node_idx = edge.source();
    //                         let service_name = self.graph.node_weight(node_idx)?;
    //                         self.services.get(service_name)
    //                     })
    //                     .collect();
    //
    //                 tracing::debug!(name = node, ?dependencies, "checking dependencies");
    //                 dbg!(&dependencies);
    //
    //                 if dependencies.iter().all(|dep| dep.is_healthy()) {
    //                     tracing::debug!(name = node, "starting service");
    //                 }
    //             }
    //         }
    //
    //         // let reversed = petgraph::visit::Reversed(&self.graph);
    //
    //         // wait for a shutdown or state change signal
    //         tracing::trace!("wait for shutdown or state change");
    //         tokio::select! {
    //             _ = shutdown_handle.changed() => {
    //                 return;
    //             }
    //             _ = self.state_change.notified() => {
    //                 tracing::trace!("state changed");
    //             }
    //         };
    //     }
    // }
}
