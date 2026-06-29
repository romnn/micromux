/// View state for one service. Domain state (execution, health, logs, healthchecks) lives in the
/// core's `SessionModelReader`; this is the per-service render cache the TUI keeps.
#[derive(Debug)]
pub struct Service {
    pub snapshot: micromux::ServiceSnapshot,
    pub cached_num_lines: u16,
    pub cached_logs: String,
    pub logs_dirty: bool,
    pub healthcheck_cached_num_lines: u16,
    pub healthcheck_cached_text: String,
    pub healthcheck_dirty: bool,
}

#[derive(Debug)]
pub struct State {
    pub services: Vec<Service>,
    pub services_sidebar_width: u16,
    pub selected_service: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            services: Vec::new(),
            services_sidebar_width: crate::style::INITIAL_SIDEBAR_WIDTH,
            selected_service: 0,
        }
    }
}

impl State {
    #[must_use]
    pub fn new(services: Vec<Service>) -> Self {
        Self {
            services,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn current_service(&self) -> Option<&Service> {
        self.services.get(self.selected_service)
    }

    #[must_use]
    pub fn current_service_mut(&mut self) -> Option<&mut Service> {
        self.services.get_mut(self.selected_service)
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
