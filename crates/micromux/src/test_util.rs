use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use yaml_spanned::Spanned;

use crate::config;
use crate::model::{LogRetention, ServiceSnapshot};
use crate::service::{RestartPolicy, StartupMode};

pub(crate) fn spanned_string(value: &str) -> Spanned<String> {
    Spanned {
        span: yaml_spanned::spanned::Span::default(),
        inner: value.to_string(),
    }
}

pub(crate) fn service_config(name: &str, command: (&str, &[&str])) -> config::Service {
    config::Service {
        name: spanned_string(name),
        startup_mode: StartupMode::Enabled,
        command: (
            spanned_string(command.0),
            command
                .1
                .iter()
                .map(|value| spanned_string(value))
                .collect(),
        ),
        working_dir: None,
        env_file: Vec::new(),
        environment: IndexMap::new(),
        depends_on: Vec::new(),
        healthcheck: None,
        ports: Vec::new(),
        restart: None,
        restart_policy: RestartPolicy::Never,
        color: None,
        log_retention: LogRetention::default(),
    }
}

pub(crate) fn unique_tmp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("micromux-{prefix}-{nanos}"))
}

pub(crate) fn initial_snapshot(id: &str) -> ServiceSnapshot {
    ServiceSnapshot::initial(
        id.to_string(),
        id.to_string(),
        Vec::new(),
        false,
        RestartPolicy::Never,
        Vec::new(),
        None,
    )
}

pub(crate) fn initial_model_entry(id: &str) -> (ServiceSnapshot, LogRetention) {
    (initial_snapshot(id), LogRetention::default())
}
