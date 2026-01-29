use crate::ServiceMap;
use color_eyre::eyre;
use petgraph::graphmap::DiGraphMap;

#[derive(Debug)]
pub struct ServiceGraph<'a> {
    pub inner: DiGraphMap<&'a str, ()>,
}

impl<'a> ServiceGraph<'a> {
    pub fn new(services: &'a ServiceMap) -> eyre::Result<ServiceGraph<'a>> {
        // Build an empty directed graph keyed by service name
        let mut graph = DiGraphMap::new();

        // Add all nodes first so dependency validation is order-independent.
        for (name, _service) in services {
            graph.add_node(name.as_str());
        }

        // Add node for each service and edges for its dependencies
        for (name, service) in services {
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
            let service = graph.node_weight(cycle.node_id()).copied().unwrap_or("<unknown>");
            eyre::eyre!(
                "cycle detected at service: `{}`",
                service
            )
        })?;

        Ok(Self { inner: graph })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::service::Service;
    use color_eyre::eyre;
    use indexmap::IndexMap;
    use std::path::Path;
    use yaml_spanned::Spanned;

    fn spanned_string(value: &str) -> Spanned<String> {
        Spanned {
            span: yaml_spanned::spanned::Span::default(),
            inner: value.to_string(),
        }
    }

    fn service_config(name: &str, depends_on: Vec<config::Dependency>) -> config::Service {
        config::Service {
            name: spanned_string(name),
            command: (spanned_string("echo"), vec![spanned_string("hi")]),
            working_dir: None,
            env_file: vec![],
            environment: IndexMap::new(),
            depends_on,
            healthcheck: None,
            ports: vec![],
            restart: None,
            color: None,
        }
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

        let graph = ServiceGraph::new(&services)?;
        assert!(graph.inner.contains_edge("b", "a"));
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
