use ratatui::style::{Style, palette::tailwind};

pub const INITIAL_SIDEBAR_WIDTH: u16 = 40;
pub const MIN_SIDEBAR_WIDTH: u16 = 20;

#[must_use]
pub fn health_style(health: Option<micromux::Health>) -> Style {
    match health {
        Some(micromux::Health::Unhealthy) => Style::default().fg(tailwind::RED.c500),
        Some(micromux::Health::Healthy) => Style::default().fg(tailwind::GREEN.c500),
        Some(micromux::Health::Unknown) => Style::default().fg(tailwind::AMBER.c500),
        None => Style::default().fg(tailwind::GREEN.c300),
    }
}

#[must_use]
pub fn service_style(snapshot: &micromux::ServiceSnapshot) -> Style {
    if snapshot.retired.is_some() {
        return Style::default().fg(tailwind::GRAY.c500);
    }
    if snapshot.desired == micromux::Desired::Disabled {
        return Style::default().fg(tailwind::GRAY.c500);
    }

    match snapshot.execution {
        // A blocked service is waiting to start, not failing — it shares the pre-start blue rather
        // than the red an `Exited` snapshot would otherwise have given it.
        micromux::Execution::Pending | micromux::Execution::Blocked => {
            Style::default().fg(tailwind::BLUE.c500)
        }
        micromux::Execution::Starting | micromux::Execution::Running => {
            health_style(snapshot.health)
        }
        // Distinct from the green "running" styling so a stopped service is obvious at a glance.
        micromux::Execution::Stopping | micromux::Execution::Unknown => {
            Style::default().fg(tailwind::AMBER.c500)
        }
        micromux::Execution::Exited => Style::default().fg(tailwind::RED.c400),
    }
}
