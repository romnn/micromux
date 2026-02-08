use crate::{HEALTHCHECK_HISTORY, state};
use micromux::Event;

fn push_log_line(
    service: &mut state::Service,
    stream: micromux::OutputStream,
    update: micromux::LogUpdateKind,
    line: String,
) {
    let line = match stream {
        micromux::OutputStream::Stdout => line,
        micromux::OutputStream::Stderr => format!("[stderr] {line}"),
    };

    match update {
        micromux::LogUpdateKind::Append => {
            service.logs.push(line);
        }
        micromux::LogUpdateKind::ReplaceLast => {
            service.logs.replace_last(line);
        }
    }
    service.logs_dirty = true;
}

fn push_healthcheck_line(
    attempt_entry: &mut state::HealthCheckAttempt,
    stream: micromux::OutputStream,
    line: String,
) {
    let line = match stream {
        micromux::OutputStream::Stdout => line,
        micromux::OutputStream::Stderr => format!("[stderr] {line}"),
    };
    attempt_entry.output.push(line);
}

#[allow(clippy::too_many_lines)]
pub fn apply(state: &mut state::State, event: Event) {
    match event {
        Event::Started { service_id } => {
            if let Some(service) = state.services.get_mut(&service_id) {
                service.exec_state = state::Execution::Running { health: None };
            }
        }
        Event::LogLine {
            service_id,
            stream,
            update,
            line,
        } => {
            if let Some(service) = state.services.get_mut(&service_id) {
                push_log_line(service, stream, update, line);
            }
        }
        Event::Killed(service_id) => {
            if let Some(service) = state.services.get_mut(&service_id)
                && service.exec_state != state::Execution::Disabled
            {
                service.exec_state = state::Execution::Killed;
            }
        }
        Event::Exited(service_id, _) => {
            if let Some(service) = state.services.get_mut(&service_id)
                && service.exec_state != state::Execution::Disabled
            {
                service.exec_state = state::Execution::Exited;
            }
        }
        Event::Healthy(service_id) => {
            if let Some(service) = state.services.get_mut(&service_id) {
                service.exec_state = state::Execution::Running {
                    health: Some(state::Health::Healthy),
                };
            }
        }
        Event::Unhealthy(service_id) => {
            if let Some(service) = state.services.get_mut(&service_id) {
                service.exec_state = state::Execution::Running {
                    health: Some(state::Health::Unhealthy),
                };
            }
        }
        Event::Disabled(service_id) => {
            if let Some(service) = state.services.get_mut(&service_id) {
                service.exec_state = state::Execution::Disabled;
            }
        }
        Event::HealthCheckStarted {
            service_id,
            attempt,
            command,
        } => {
            if let Some(service) = state.services.get_mut(&service_id) {
                while service.healthcheck_attempts.len() >= HEALTHCHECK_HISTORY {
                    service.healthcheck_attempts.pop_front();
                }

                service
                    .healthcheck_attempts
                    .push_back(state::HealthCheckAttempt {
                        id: attempt,
                        command,
                        output: micromux::BoundedLog::with_limits(200, 256 * crate::KIB),
                        result: None,
                    });
                service.healthcheck_dirty = true;
            }
        }
        Event::HealthCheckLogLine {
            service_id,
            attempt,
            stream,
            line,
        } => {
            if let Some(service) = state.services.get_mut(&service_id)
                && let Some(attempt_entry) = service
                    .healthcheck_attempts
                    .iter_mut()
                    .find(|a| a.id == attempt)
            {
                push_healthcheck_line(attempt_entry, stream, line);
                service.healthcheck_dirty = true;
            }
        }
        Event::HealthCheckFinished {
            service_id,
            attempt,
            success,
            exit_code,
        } => {
            if let Some(service) = state.services.get_mut(&service_id)
                && let Some(attempt_entry) = service
                    .healthcheck_attempts
                    .iter_mut()
                    .find(|a| a.id == attempt)
            {
                attempt_entry.result = Some(state::HealthCheckResult { success, exit_code });
                service.healthcheck_dirty = true;
            }
        }
    }
}
