//! Attach-mode startup and diagnostics.

use std::fmt::Write as _;

use micromux_control::{
    AmbiguousSelection, ControlError, ProbeReport, SelectError, SessionSelector, SocketProbeDetail,
    SocketProbeStatus,
};

use crate::options::Options;

/// Resolve a live session and run the existing TUI over its remote mirror.
pub(crate) async fn run(options: &Options, raw_session: Option<&str>) -> Result<(), crate::Error> {
    if !micromux_control::transport_supported() {
        return Err(crate::Error::Message(
            "the micromux control plane is not supported on this platform".to_string(),
        ));
    }

    let cwd = std::env::current_dir()?;
    let selector = raw_session.map(SessionSelector::parse);
    let resolved =
        micromux_control::resolve_selector(&cwd, selector, options.config_path.as_deref())
            .await
            .map_err(|error| crate::Error::Message(format_select_error(error)))?;
    let remote = micromux_tui::RemoteSource::connect(resolved.endpoint)
        .await
        .map_err(|error| crate::Error::Message(format_control_error(error)))?;

    let shutdown = micromux::CancellationToken::new();
    crate::spawn_shutdown_handler(shutdown.clone());
    let _log_guard = crate::setup_logging(options)?;
    let app = micromux_tui::App::new(
        micromux_tui::SessionSource::Remote(remote),
        None,
        shutdown,
        !options.no_pretty_json_logs,
    );
    Ok(app.render().await?)
}

fn format_select_error(error: SelectError) -> String {
    match error {
        SelectError::NoSession(report) => format_no_session(&report),
        SelectError::Ambiguous(ambiguous) => format_ambiguous(&ambiguous),
        SelectError::Busy(message) if message.contains("protocol mismatch") => format!(
            "{message}\nupgrade one side so the attach client and session use compatible control protocol versions"
        ),
        SelectError::Busy(message) => format!("{message}\ntry again, or choose another session"),
        SelectError::Unsupported => {
            "the micromux control plane is not supported on this platform".to_string()
        }
        SelectError::Control(error) => format_control_error(error),
    }
}

fn format_no_session(report: &ProbeReport) -> String {
    let mut message = report.summary.clone();
    if let Some(config_path) = &report.config_path {
        let _ = write!(message, "\nconfig: {config_path}");
    }
    if !report.runtime_dirs.is_empty() {
        message.push_str("\nruntime directories:");
        for runtime_dir in &report.runtime_dirs {
            let status = if runtime_dir.usable {
                "usable".to_string()
            } else {
                format!(
                    "unusable: {}",
                    runtime_dir.error.as_deref().unwrap_or("unknown error")
                )
            };
            let _ = write!(message, "\n  {} ({status})", runtime_dir.path);
        }
    }
    if !report.socket_probes.is_empty() {
        message.push_str("\nsocket probes:");
        for probe in &report.socket_probes {
            let _ = write!(message, "\n  {}", format_probe(probe));
        }
    }
    message.push_str(
        "\nstart the session with `micromux serve` or the MCP `start_session` tool, then attach again",
    );
    message
}

fn format_probe(probe: &SocketProbeDetail) -> String {
    let status = match probe.status {
        SocketProbeStatus::Session => "session",
        SocketProbeStatus::Absent => "absent",
        SocketProbeStatus::ProtocolMismatch => "protocol mismatch",
        SocketProbeStatus::Unreachable => "unreachable",
    };
    match &probe.message {
        Some(detail) => format!("{} -> {status} ({detail})", probe.endpoint),
        None => format!("{} -> {status}", probe.endpoint),
    }
}

fn format_ambiguous(ambiguous: &AmbiguousSelection) -> String {
    let mut message = ambiguous.message.clone();
    message.push_str("\nmatching sessions:");
    for session in &ambiguous.candidates {
        let _ = write!(
            message,
            "\n  {} (pid {}, config {})\n    selectors: --session pid:{} | --session name:{} | --session hash:{}",
            session.name, session.pid, session.config_path, session.pid, session.name, session.id,
        );
    }
    message
}

fn format_control_error(error: ControlError) -> String {
    match error {
        ControlError::ProtocolMismatch { peer, ours } => format!(
            "control protocol version mismatch: session={peer}, attach_client={ours}; upgrade one side"
        ),
        other => format!("failed to connect the attach client: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use micromux_control::{PROTOCOL_VERSION, SessionInfo};

    #[test]
    fn ambiguous_diagnostic_lists_concrete_selectors() {
        let message = format_ambiguous(&AmbiguousSelection {
            message: "matched two".to_string(),
            candidates: vec![SessionInfo {
                protocol_version: PROTOCOL_VERSION,
                id: "abc123".to_string(),
                pid: 42,
                start_time: 1,
                name: "api".to_string(),
                working_dir: "/project".to_string(),
                config_path: "/project/micromux.yaml".to_string(),
                services: Vec::new(),
                micromux_version: "test".to_string(),
                capabilities: None,
            }],
        });

        assert!(message.contains("--session pid:42"));
        assert!(message.contains("--session name:api"));
        assert!(message.contains("--session hash:abc123"));
    }
}
