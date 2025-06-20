use crate::{config::Config, scheduler::ServiceID, service::Service};
use color_eyre::eyre;
use petgraph::{Graph, graph::NodeIndex, graphmap::DiGraphMap};
use std::collections::HashMap;

#[derive(Debug)]
pub struct ServiceGraph<'a> {
    pub inner: DiGraphMap<&'a str, ()>,
}

impl<'a> ServiceGraph<'a> {
    // pub fn new(config: &'a Config) -> eyre::Result<ServiceGraph<'a>> {
    pub fn new(services: &'a HashMap<ServiceID, Service>) -> eyre::Result<ServiceGraph<'a>> {
        // Build an empty directed graph keyed by service name
        let mut graph = DiGraphMap::new();

        // Add node for each service and edges for its dependencies
        for (name, service) in services.iter() {
            graph.add_node(name.as_str());
            let name = name.as_ref();
            for dep in &service.depends_on {
                let dep_name = dep.name.as_ref().as_str();

                // Validate that the dependency actually exists
                if !graph.contains_node(dep_name) {
                    return Err(eyre::eyre!(
                        "service `{}` depends on unknown `{}`",
                        name,
                        dep_name
                    ));
                }
                graph.add_edge(dep_name, name, ());
            }
        }

        // Ensure there are no cycles
        petgraph::algo::toposort(&graph, None).map_err(|cycle| {
            use petgraph::data::DataMap;
            eyre::eyre!(
                "cycle detected at service: `{}`",
                graph.node_weight(cycle.node_id()).unwrap()
            )
        })?;

        Ok(Self { inner: graph })
    }
}

#[derive(Debug)]
pub struct OwnedServiceGraph {
    pub inner: Graph<String, ()>,
}

impl OwnedServiceGraph {
    pub fn new(config: &Config) -> eyre::Result<Self> {
        // Build a directed graph where nodes are services.
        // We use a HashMap to map service names to their node indices.
        // let mut graph: Graph<&Service, ()> = Graph::default();
        let mut graph: Graph<String, ()> = Graph::default();
        let mut nodes: HashMap<String, NodeIndex> = HashMap::new();

        // Add each service as a node.
        for (name, service) in &config.services {
            let node = graph.add_node(name.as_ref().clone());
            nodes.insert(name.as_ref().clone(), node);
        }

        // add edges from each dependency to the service.
        // let mut graph: DiGraphMap<String, ()> = DiGraphMap::default();
        for (name, service) in &config.services {
            // graph.add_node(name.clone());
            // for dep in &service.depends_on {
            //     graph.add_edge(dep.clone(), name.clone(), ());
            // }
            let service_node = nodes[name.as_ref()];
            for dep in &service.depends_on {
                if let Some(dep_node) = nodes.get(dep.name.as_ref()) {
                    graph.add_edge(*dep_node, service_node, ());
                } else {
                    return Err(eyre::eyre!(
                        "service {:?} depends on unknown service '{}'",
                        name.as_ref(),
                        dep.name.as_ref(),
                    ));
                }
            }
        }

        dbg!(&graph);

        // check for circles
        petgraph::algo::toposort(&graph, None)
            .map_err(|cycle| eyre::eyre!("cycle detected at node: {:?}", graph[cycle.node_id()]))?;
        Ok(Self { inner: graph })
    }
}
