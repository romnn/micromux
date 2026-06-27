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
            service.live_snapshot_id = None;
            service.logs.push(line);
        }
        micromux::LogUpdateKind::ReplaceLast => {
            service.live_snapshot_id = None;
            service.logs.replace_last(line);
        }
        micromux::LogUpdateKind::LiveSnapshot { id } => {
            if service.live_snapshot_id == Some(id) {
                service.logs.replace_last(line);
            } else {
                service.logs.push(line);
                service.live_snapshot_id = Some(id);
            }
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
                // Started is forwarded before any new-run log line, so a live snapshot from the
                // new run must append instead of replacing the previous run's final frame.
                service.live_snapshot_id = None;
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
        // A late health event from a probe that was racing a disable must not flip a disabled
        // service back to Running. (Started is intentionally not guarded: Enable relies on the
        // subsequent Started event to clear the Disabled state.)
        Event::Healthy(service_id) => {
            if let Some(service) = state.services.get_mut(&service_id)
                && service.exec_state != state::Execution::Disabled
            {
                service.exec_state = state::Execution::Running {
                    health: Some(state::Health::Healthy),
                };
            }
        }
        Event::Unhealthy(service_id) => {
            if let Some(service) = state.services.get_mut(&service_id)
                && service.exec_state != state::Execution::Disabled
            {
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
        Event::ClearLogs(service_id) => {
            if let Some(service) = state.services.get_mut(&service_id) {
                service.logs.clear();
                service.live_snapshot_id = None;
                service.cached_num_lines = 0;
                service.cached_logs.clear();
                service.logs_dirty = true;
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

#[cfg(test)]
mod tests {
    use super::*;
    use micromux::{BoundedLog, LogUpdateKind, OutputStream};
    use similar_asserts::assert_eq;
    use std::collections::VecDeque;

    fn service() -> state::Service {
        state::Service {
            id: "svc".to_string(),
            exec_state: state::Execution::Pending,
            open_ports: Vec::new(),
            logs: BoundedLog::with_limits(100, 1024).into(),
            live_snapshot_id: None,
            cached_num_lines: 0,
            cached_logs: String::new(),
            logs_dirty: false,
            healthcheck_configured: false,
            healthcheck_attempts: VecDeque::new(),
            healthcheck_cached_num_lines: 0,
            healthcheck_cached_text: String::new(),
            healthcheck_dirty: false,
        }
    }

    fn state_with_service() -> state::State {
        let mut services = indexmap::IndexMap::new();
        services.insert("svc".to_string(), service());
        state::State::new(services)
    }

    fn service_text(service: &state::Service) -> String {
        let (_, text) = service.logs.full_text();
        text
    }

    #[test]
    fn live_snapshot_updates_the_same_log_entry() {
        let mut service = service();
        push_log_line(
            &mut service,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 7 },
            "frame one".to_string(),
        );
        push_log_line(
            &mut service,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 7 },
            "frame two".to_string(),
        );

        assert_eq!(service_text(&service), "frame two");
    }

    #[test]
    fn live_snapshot_with_new_id_starts_new_line() {
        let mut service = service();
        push_log_line(
            &mut service,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 7 },
            "frame one".to_string(),
        );
        push_log_line(
            &mut service,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 8 },
            "frame two".to_string(),
        );

        assert_eq!(service_text(&service), "frame one\nframe two");
    }

    #[test]
    fn live_snapshot_appends_when_target_is_absent() {
        let mut service = service();
        push_log_line(
            &mut service,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "ordinary".to_string(),
        );
        push_log_line(
            &mut service,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 7 },
            "frame".to_string(),
        );

        assert_eq!(service_text(&service), "ordinary\nframe");
    }

    #[test]
    fn append_resets_live_snapshot_target() {
        let mut service = service();
        push_log_line(
            &mut service,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 7 },
            "frame one".to_string(),
        );
        push_log_line(
            &mut service,
            OutputStream::Stdout,
            LogUpdateKind::Append,
            "rate limited".to_string(),
        );
        push_log_line(
            &mut service,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 7 },
            "frame two".to_string(),
        );

        assert_eq!(service_text(&service), "frame one\nrate limited\nframe two");
    }

    #[test]
    fn replace_last_resets_live_snapshot_target() {
        let mut service = service();
        push_log_line(
            &mut service,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 7 },
            "frame one".to_string(),
        );
        push_log_line(
            &mut service,
            OutputStream::Stdout,
            LogUpdateKind::ReplaceLast,
            "replacement".to_string(),
        );
        push_log_line(
            &mut service,
            OutputStream::Stdout,
            LogUpdateKind::LiveSnapshot { id: 7 },
            "frame two".to_string(),
        );

        assert_eq!(service_text(&service), "replacement\nframe two");
    }

    #[test]
    fn clear_logs_resets_live_snapshot_target() {
        let mut state = state_with_service();
        apply(
            &mut state,
            Event::LogLine {
                service_id: "svc".to_string(),
                stream: OutputStream::Stdout,
                update: LogUpdateKind::LiveSnapshot { id: 7 },
                line: "frame one".to_string(),
            },
        );
        apply(&mut state, Event::ClearLogs("svc".to_string()));
        apply(
            &mut state,
            Event::LogLine {
                service_id: "svc".to_string(),
                stream: OutputStream::Stdout,
                update: LogUpdateKind::LiveSnapshot { id: 7 },
                line: "frame two".to_string(),
            },
        );

        let text = state.services.get("svc").map(service_text);
        assert_eq!(text.as_deref(), Some("frame two"));
    }

    #[test]
    fn started_resets_live_snapshot_target() {
        let mut state = state_with_service();
        apply(
            &mut state,
            Event::LogLine {
                service_id: "svc".to_string(),
                stream: OutputStream::Stdout,
                update: LogUpdateKind::LiveSnapshot { id: 7 },
                line: "run one".to_string(),
            },
        );
        apply(
            &mut state,
            Event::Started {
                service_id: "svc".to_string(),
            },
        );
        apply(
            &mut state,
            Event::LogLine {
                service_id: "svc".to_string(),
                stream: OutputStream::Stdout,
                update: LogUpdateKind::LiveSnapshot { id: 7 },
                line: "run two".to_string(),
            },
        );

        let text = state.services.get("svc").map(service_text);
        assert_eq!(text.as_deref(), Some("run one\nrun two"));
    }
}
