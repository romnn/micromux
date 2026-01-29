use std::collections::VecDeque;

use micromux::{AsyncBoundedLog, BoundedLog, ServiceID};

#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::Display, strum::IntoStaticStr)]
pub enum Health {
    #[strum(serialize = "HEALTHY")]
    Healthy,
    #[strum(serialize = "UNHEALTHY")]
    Unhealthy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::Display, strum::IntoStaticStr)]
pub enum Execution {
    #[strum(serialize = "DISABLED")]
    Disabled,
    #[strum(serialize = "PENDING")]
    Pending,
    #[strum(serialize = "RUNNING")]
    Running { health: Option<Health> },
    #[strum(serialize = "KILLED")]
    Killed,
    #[strum(serialize = "EXITED")]
    Exited,
}

#[derive(Debug)]
pub struct Service {
    pub id: ServiceID,
    pub exec_state: Execution,
    pub open_ports: Vec<u16>,
    pub logs: AsyncBoundedLog,
    pub cached_num_lines: u16,
    pub cached_logs: String,
    pub logs_dirty: bool,
    pub healthcheck_configured: bool,
    pub healthcheck_attempts: VecDeque<HealthCheckAttempt>,
    pub healthcheck_cached_num_lines: u16,
    pub healthcheck_cached_text: String,
    pub healthcheck_dirty: bool,
}

#[derive(Debug)]
pub struct HealthCheckAttempt {
    pub id: u64,
    pub command: String,
    pub output: BoundedLog,
    pub result: Option<HealthCheckResult>,
}

#[derive(Debug, Clone, Copy)]
pub struct HealthCheckResult {
    pub success: bool,
    pub exit_code: i32,
}

#[derive(Debug)]
pub struct State {
    pub services: indexmap::IndexMap<ServiceID, Service>,
    pub services_sidebar_width: u16,
    pub selected_service: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            services: indexmap::IndexMap::new(),
            services_sidebar_width: crate::style::INITIAL_SIDEBAR_WIDTH,
            selected_service: 0,
        }
    }
}

impl State {
    #[must_use]
    pub fn new(services: indexmap::IndexMap<ServiceID, Service>) -> Self {
        Self {
            services,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn current_service(&self) -> Option<&Service> {
        self.services
            .get_index(self.selected_service)
            .map(|(_id, service)| service)
    }

    #[must_use]
    pub fn current_service_mut(&mut self) -> Option<&mut Service> {
        self.services
            .get_index_mut(self.selected_service)
            .map(|(_id, service)| service)
    }

    /// Update the selection index.
    pub fn service_down(&mut self) {
        self.selected_service = self
            .selected_service
            .saturating_add(1)
            .min(self.services.len().saturating_sub(1));
    }

    pub fn service_up(&mut self) {
        self.selected_service = self.selected_service.saturating_sub(1);
    }

    pub fn resize_left(&mut self) {
        self.services_sidebar_width = self
            .services_sidebar_width
            .saturating_sub(2)
            .max(crate::style::MIN_SIDEBAR_WIDTH);
    }

    pub fn resize_right(&mut self) {
        self.services_sidebar_width = self.services_sidebar_width.saturating_add(2);
    }
}
