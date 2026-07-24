use crate::ServiceMap;
use petgraph::graphmap::DiGraphMap;

/// Errors from validating service dependencies.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A dependency names a service that is not configured.
    #[error("service `{service}` depends on unknown `{dependency}`")]
    UnknownDependency {
        /// Service declaring the dependency.
        service: String,
        /// Missing dependency id.
        dependency: String,
    },
    /// The dependency graph contains a cycle.
    #[error("cycle detected at service: `{service}`")]
    Cycle {
        /// A service participating in the cycle.
        service: String,
    },
}

#[derive(Debug)]
pub struct ServiceGraph;

impl ServiceGraph {
    pub fn new(services: &ServiceMap) -> Result<ServiceGraph, Error> {
        // Build an empty directed graph keyed by service name
        let mut graph: DiGraphMap<&str, ()> = DiGraphMap::new();

        // Add all nodes first so dependency validation is order-independent.
        for (name, _service) in services {
            graph.add_node(name.as_str());
        }

        // Add node for each service and edges for its dependencies
        for (name, service) in services {
            let name: &str = name.as_ref();
            for dep in &service.spec.depends_on {
                let dep_name = dep.service.as_str();

                // Validate that the dependency actually exists
                if !graph.contains_node(dep_name) {
                    return Err(Error::UnknownDependency {
                        service: name.to_string(),
                        dependency: dep_name.to_string(),
                    });
                }
                graph.add_edge(dep_name, name, ());
            }
        }

        // Ensure there are no cycles
        petgraph::algo::toposort(&graph, None).map_err(|cycle| {
            use petgraph::data::DataMap;
            let service = graph
                .node_weight(cycle.node_id())
                .copied()
                .unwrap_or("<unknown>");
            Error::Cycle {
                service: service.to_string(),
            }
        })?;

        Ok(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::service::Service;
    use crate::test_util::spanned_string;
    use color_eyre::eyre;
    use std::path::Path;

    fn service_config(name: &str, depends_on: Vec<config::Dependency>) -> config::Service {
        let mut service = crate::test_util::service_config(name, ("echo", &["hi"]));
        service.depends_on = depends_on;
        service
    }

    #[test]
    fn graph_dependency_order_is_independent() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let dep = config::Dependency {
            name: spanned_string("b"),
            condition: None,
        };

        // Insert "a" before "b" to ensure graph creation does not depend on iteration order.
        let mut services: ServiceMap = ServiceMap::new();
        services.insert(
            "a".to_string(),
            Service::new("a", config_dir, service_config("a", vec![dep]))?,
        );
        services.insert(
            "b".to_string(),
            Service::new("b", config_dir, service_config("b", vec![]))?,
        );

        ServiceGraph::new(&services)?;
        Ok(())
    }

    #[test]
    fn graph_unknown_dependency_is_error() -> eyre::Result<()> {
        let config_dir = Path::new(".");
        let dep = config::Dependency {
            name: spanned_string("missing"),
            condition: None,
        };

        let mut services: ServiceMap = ServiceMap::new();
        let service = Service::new("a", config_dir, service_config("a", vec![dep]))?;
        services.insert("a".to_string(), service);

        let res = ServiceGraph::new(&services);
        assert!(res.is_err());
        Ok(())
    }
}
