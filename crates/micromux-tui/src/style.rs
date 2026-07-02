use ratatui::style::{Color, Style, palette::tailwind};

pub const INITIAL_SIDEBAR_WIDTH: u16 = 40;
pub const MIN_SIDEBAR_WIDTH: u16 = 20;

#[must_use]
pub fn health_style(health: Option<micromux::Health>) -> Style {
    match health {
        Some(micromux::Health::Unhealthy) => Style::default().fg(tailwind::RED.c500),
        Some(micromux::Health::Healthy) => {
            Style::default().fg(Color::White).fg(tailwind::GREEN.c500)
        }
        None => Style::default().fg(Color::White).fg(tailwind::GREEN.c300),
    }
}

#[must_use]
pub fn service_style(snapshot: &micromux::ServiceSnapshot) -> Style {
    if snapshot.desired == micromux::Desired::Disabled {
        return Style::default().fg(Color::White).fg(tailwind::GRAY.c500);
    }

    match snapshot.execution {
        micromux::Execution::Pending => Style::default().fg(Color::White).fg(tailwind::BLUE.c500),
        micromux::Execution::Starting | micromux::Execution::Running => {
            health_style(snapshot.health)
        }
        // Distinct from the green "running" styling so a stopped service is obvious at a glance.
        micromux::Execution::Stopping => Style::default().fg(tailwind::AMBER.c500),
        micromux::Execution::Exited => Style::default().fg(tailwind::RED.c400),
    }
}
