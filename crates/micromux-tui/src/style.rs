use crate::state;
use ratatui::style::{Color, Modifier, Style, palette::tailwind};

pub const INITIAL_SIDEBAR_WIDTH: u16 = 40;
pub const MIN_SIDEBAR_WIDTH: u16 = 20;

pub fn health_style(health: Option<state::Health>) -> Style {
    match health {
        Some(state::Health::Unhealthy) => Style::default().fg(tailwind::RED.c500),
        Some(state::Health::Healthy) => Style::default().fg(Color::White).fg(tailwind::GREEN.c500),
        None => Style::default().fg(Color::White).fg(tailwind::GREEN.c300),
    }
}

pub fn service_style(state: state::Execution) -> Style {
    match state {
        state::Execution::Disabled => Style::default().fg(Color::White).fg(tailwind::GRAY.c500),
        state::Execution::Pending => Style::default().fg(Color::White).fg(tailwind::BLUE.c500),
        state::Execution::Running { health, .. } => health_style(health),
        state::Execution::Killed { .. } | state::Execution::Exited { .. } => health_style(None),
    }
}
