//! What a running session tells agents and the TUI about its identity, URLs, and ports.

use std::collections::BTreeSet;
use std::sync::Arc;

use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::{ResourceStore, ResourceView, Route};
use crate::scheduler::ServiceID;

/// A session's name, browser URLs, and allocated ports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSummary {
    /// Best-effort branch and cached open PR context; absent outside a named Git branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<super::GitContext>,
    /// Session name, such as `luup2`.
    pub name: String,
    /// Domain every route lives under, such as `luup2.localhost`.
    pub domain: String,
    /// What follows `<route>.` in a browser URL, such as `luup2.localhost` or
    /// `luup2.localhost:8080`.
    pub host_suffix: String,
    /// Port of the shared proxy serving the routes.
    pub proxy_port: u16,
    /// The routes with their browser URLs, in config order.
    #[serde(default)]
    pub routes: Vec<Route>,
    /// The port allocated to every `allocate_ports` name, in config order.
    #[serde(default)]
    pub allocated_ports: IndexMap<String, u16>,
    /// Whether the proxy currently serves the routes; absent when the session has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingStatus>,
    /// Holder and queue of every resource the session uses.
    ///
    /// [`SessionStatus::current`] leaves it empty; callers that display reservations fill it
    /// from [`SessionStatus::reservations`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reservations: Vec<ResourceView>,
}

/// Whether the shared proxy currently serves a session's routes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RoutingStatus {
    /// The routes are not registered yet.
    Pending,
    /// The proxy serves the routes.
    Registered,
    /// The routes are not served right now; the session keeps trying to restore them.
    Unavailable {
        /// Why, for display.
        reason: String,
    },
}

/// The live [`SessionSummary`] of a running session, shared by its adapters.
///
/// Cloning it is cheap; every clone observes and updates the same summary.
#[derive(Debug, Clone)]
pub struct SessionStatus {
    pub(super) git_metadata_enabled: bool,
    summary: Arc<watch::Sender<SessionSummary>>,
    /// The session's reservations, read for display; `None` without a config file.
    resources: Option<Arc<ResourceStore>>,
    /// The resources the session holds, waits for, or has services that need, as the scheduler
    /// last published them.
    watched_resources: Arc<parking_lot::Mutex<BTreeSet<String>>>,
}

impl SessionStatus {
    pub(crate) fn new(summary: SessionSummary, resources: Option<ResourceStore>) -> Self {
        Self {
            git_metadata_enabled: true,
            summary: Arc::new(watch::Sender::new(summary)),
            resources: resources.map(Arc::new),
            watched_resources: Arc::default(),
        }
    }

    /// The current summary.
    #[must_use]
    pub fn current(&self) -> SessionSummary {
        self.summary.borrow().clone()
    }

    /// Records whether the proxy currently serves the session's routes.
    ///
    /// Ignored for a session without routes.
    pub fn set_routing(&self, routing: RoutingStatus) {
        self.summary.send_if_modified(|summary| {
            if summary.routes.is_empty() || summary.routing.as_ref() == Some(&routing) {
                return false;
            }
            summary.routing = Some(routing);
            true
        });
    }

    /// Sets each route's owning service to `owner` of its port, notifying subscribers only when an
    /// owner changed.
    pub(crate) fn set_route_owners(&self, owner: impl Fn(u16) -> Option<ServiceID>) {
        self.summary.send_if_modified(|summary| {
            let mut changed = false;
            for route in &mut summary.routes {
                let service = owner(route.port);
                if route.service != service {
                    route.service = service;
                    changed = true;
                }
            }
            changed
        });
    }

    /// Records the resources the session holds, waits for, or has services that need.
    pub(crate) fn set_watched_resources(&self, resources: BTreeSet<String>) {
        *self.watched_resources.lock() = resources;
    }

    /// The holder and queue of every resource the session uses, as the reservation files were
    /// last written.
    ///
    /// It neither takes the registry lock nor writes, so it may still show claims of sessions
    /// that died until a session normalizes the files. A file that cannot be read is left out.
    #[must_use]
    pub fn reservations(&self) -> Vec<ResourceView> {
        let Some(store) = &self.resources else {
            return Vec::new();
        };
        let watched = self.watched_resources.lock().clone();
        watched
            .iter()
            .filter_map(|resource| {
                store
                    .written_view(resource)
                    .inspect_err(
                        |err| tracing::warn!(%err, resource, "cannot read the reservation"),
                    )
                    .ok()
            })
            .collect()
    }

    /// Follows changes to the summary.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<SessionSummary> {
        self.summary.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use similar_asserts::assert_eq;

    fn summary(routes: Vec<Route>) -> SessionSummary {
        SessionSummary {
            git: None,
            name: "one".to_string(),
            domain: "one.localhost".to_string(),
            host_suffix: "one.localhost".to_string(),
            proxy_port: 80,
            routing: (!routes.is_empty()).then_some(RoutingStatus::Pending),
            routes,
            allocated_ports: IndexMap::new(),
            reservations: Vec::new(),
        }
    }

    #[test]
    fn summaries_from_older_sessions_have_no_git_context() -> eyre::Result<()> {
        let mut value = serde_json::to_value(summary(Vec::new()))?;
        if let Some(fields) = value.as_object_mut() {
            fields.remove("git");
        }
        let decoded: SessionSummary = serde_json::from_value(value)?;
        assert_eq!(decoded.git, None);
        Ok(())
    }

    /// Routing updates reach subscribers, and a session without routes never reports any.
    #[test]
    fn routing_status_only_applies_to_sessions_with_routes() {
        let without = SessionStatus::new(summary(Vec::new()), None);
        without.set_routing(RoutingStatus::Registered);
        assert_eq!(without.current().routing, None);

        let with = SessionStatus::new(
            summary(vec![Route {
                host: "app".to_string(),
                path: None,
                strip_prefix: false,
                url: "http://app.one.localhost/".to_string(),
                port: 20001,
                service: None,
            }]),
            None,
        );
        let changes = with.subscribe();
        with.set_routing(RoutingStatus::Registered);
        assert!(changes.has_changed().unwrap_or(false));
        assert_eq!(with.current().routing, Some(RoutingStatus::Registered));
    }
}
