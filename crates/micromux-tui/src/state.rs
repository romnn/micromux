/// View state for one service. Domain state (execution, health, logs, healthchecks) lives in the
/// [`crate::SessionSource`]; this is the per-service render cache the TUI keeps.
#[derive(Debug)]
pub struct Service {
    pub snapshot: micromux::ServiceSnapshot,
    pub cached_lines: std::collections::VecDeque<(u64, String)>,
    pub cached_text: ratatui::text::Text<'static>,
    pub text_dirty: bool,
    pub cached_wrapped_lines: usize,
    pub cached_wrap: Option<(bool, u16)>,
    pub logs_dirty: bool,
    pub healthcheck_cached_num_lines: usize,
    pub healthcheck_cached_text: ratatui::text::Text<'static>,
    pub healthcheck_cached_wrap: Option<(bool, u16)>,
    pub healthcheck_dirty: bool,
}

impl Service {
    pub(crate) fn new(snapshot: micromux::ServiceSnapshot) -> Self {
        Self {
            snapshot,
            cached_lines: std::collections::VecDeque::new(),
            cached_text: ratatui::text::Text::default(),
            text_dirty: true,
            cached_wrapped_lines: 0,
            cached_wrap: None,
            logs_dirty: true,
            healthcheck_cached_num_lines: 0,
            healthcheck_cached_text: ratatui::text::Text::default(),
            healthcheck_cached_wrap: None,
            healthcheck_dirty: true,
        }
    }
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
        let minimum = self
            .services_sidebar_width
            .min(crate::style::MIN_SIDEBAR_WIDTH);
        self.services_sidebar_width = self.services_sidebar_width.saturating_sub(2).max(minimum);
    }

    pub fn resize_right(&mut self, max_width: u16) {
        if max_width < crate::style::MIN_SIDEBAR_WIDTH {
            self.services_sidebar_width =
                self.services_sidebar_width.saturating_add(2).min(max_width);
            return;
        }
        self.services_sidebar_width = self
            .services_sidebar_width
            .saturating_add(2)
            .clamp(crate::style::MIN_SIDEBAR_WIDTH, max_width);
    }

    pub fn clamp_sidebar(&mut self, max_width: u16) {
        self.services_sidebar_width = if max_width < crate::style::MIN_SIDEBAR_WIDTH {
            self.services_sidebar_width.min(max_width)
        } else {
            self.services_sidebar_width
                .clamp(crate::style::MIN_SIDEBAR_WIDTH, max_width)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::State;
    use similar_asserts::assert_eq;

    #[test]
    fn sidebar_never_exceeds_a_narrow_terminal() {
        let mut state = State::default();

        state.clamp_sidebar(5);
        assert_eq!(state.services_sidebar_width, 5);
        state.resize_right(5);
        assert_eq!(state.services_sidebar_width, 5);
        state.resize_left();
        assert_eq!(state.services_sidebar_width, 5);
        state.services_sidebar_width = 3;
        state.resize_right(5);
        assert_eq!(state.services_sidebar_width, 5);
    }
}
