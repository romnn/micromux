//! Hostname routes a session registers with the shared proxy.

use std::collections::HashMap;

use codespan_reporting::diagnostic::{Diagnostic, Label};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::SessionContext;
use crate::diagnostics::{Span, ToDiagnostics};
use crate::env::{self, InterpolationError};
use crate::scheduler::ServiceID;

/// Host of the catch-all route, which serves every hostname under the session domain that no
/// other route matches.
pub const CATCH_ALL_HOST: &str = "*";

/// A route with its upstream port resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Route {
    /// Hostname labels below the session domain, such as `app`, or `*` for the catch-all.
    pub host: String,
    /// Browser URL of the route; for the catch-all, the bare session domain.
    pub url: String,
    /// Upstream port on 127.0.0.1.
    pub port: u16,
    /// The service whose `ports` list contains the upstream port, if exactly one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceID>,
}

/// A route whose port cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouteError {
    /// The port value references a variable that cannot be substituted.
    #[error("cannot resolve the port of route `{host}`: {source}")]
    Interpolation {
        /// The route's host.
        host: String,
        /// Location of the port value.
        span: Span,
        /// The failing reference.
        #[source]
        source: InterpolationError,
    },
    /// The port value is not a nonzero port number.
    #[error("the port of route `{host}` resolves to `{value}`, which is not a port number")]
    InvalidPort {
        /// The route's host.
        host: String,
        /// Location of the port value.
        span: Span,
        /// The resolved value.
        value: String,
    },
    /// The port is the proxy's own, so every request would loop back into the proxy.
    #[error("route `{host}` points at port {port}, which is the proxy's own port")]
    ProxyPort {
        /// The route's host.
        host: String,
        /// Location of the port value.
        span: Span,
        /// The offending port.
        port: u16,
    },
}

impl RouteError {
    fn span(&self) -> &Span {
        match self {
            Self::Interpolation { span, .. }
            | Self::InvalidPort { span, .. }
            | Self::ProxyPort { span, .. } => span,
        }
    }
}

impl ToDiagnostics for RouteError {
    fn to_diagnostics<F: Copy + PartialEq>(&self, file_id: F) -> Vec<Diagnostic<F>> {
        let mut diagnostic = Diagnostic::error()
            .with_message(self.to_string())
            .with_labels(vec![
                Label::primary(file_id, self.span().clone()).with_message("route port"),
            ]);
        if matches!(self, Self::Interpolation { .. }) {
            diagnostic = diagnostic.with_notes(vec![
                "route ports see micromux's own environment and the session variables, such as \
                 `allocate_ports` names, but not service env files"
                    .to_string(),
            ]);
        }
        vec![diagnostic]
    }
}

/// The service whose `ports` list contains `port`, if exactly one of `services` does.
///
/// A session that starts passes every configured service; after a config reload the scheduler
/// passes the live configured ones, so ownership always follows the config the session runs.
pub(crate) fn owner<'a>(
    port: u16,
    services: impl IntoIterator<Item = (&'a ServiceID, &'a crate::service::Service)>,
) -> Option<ServiceID> {
    let mut owners = services
        .into_iter()
        .filter(|(_, service)| service.spec.ports.contains(&port))
        .map(|(id, _)| id.clone());
    match (owners.next(), owners.next()) {
        (Some(owner), None) => Some(owner),
        _ => None,
    }
}

/// Resolves every configured route of `session`.
///
/// Ports see micromux's own environment with the session variables pinned on top, not any
/// service's env files, because routes belong to the session rather than to one service.
///
/// # Errors
///
/// Returns every route that cannot be resolved.
pub(crate) fn resolve(
    session: &SessionContext,
    services: &crate::ServiceMap,
    supervisor_environment: &HashMap<String, String>,
) -> Result<Vec<Route>, Vec<RouteError>> {
    let environment = session.environment.layered_over(supervisor_environment);
    let suffix = super::host_suffix(&session.identity, session.proxy_port);
    let mut routes = Vec::new();
    let mut errors = Vec::new();
    for route in &session.settings.routes {
        let host = route.host.inner.clone();
        let span: Span = route.port.span.into();
        let value = match env::interpolate(&route.port.inner, &environment) {
            Ok(value) => value,
            Err(source) => {
                errors.push(RouteError::Interpolation { host, span, source });
                continue;
            }
        };
        let port = match value.trim().parse::<u16>() {
            Ok(port) if port != 0 => port,
            _ => {
                errors.push(RouteError::InvalidPort { host, span, value });
                continue;
            }
        };
        if port == session.proxy_port {
            errors.push(RouteError::ProxyPort { host, span, port });
            continue;
        }
        let service = owner(port, services);
        let url = if host == CATCH_ALL_HOST {
            format!("http://{suffix}/")
        } else {
            format!("http://{host}.{suffix}/")
        };
        routes.push(Route {
            host,
            url,
            port,
            service,
        });
    }
    if errors.is_empty() {
        Ok(routes)
    } else {
        Err(errors)
    }
}
