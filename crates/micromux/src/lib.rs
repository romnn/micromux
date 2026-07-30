//! `micromux` is a small process supervisor with a terminal UI.
//!
//! The crate provides the core scheduling and service-management logic used by the CLI and TUI.
//! Most users will interact with it through [`Micromux`] and configuration parsing via
//! [`from_str`].
//!
//! # Examples
//!
//! Parse a configuration file and construct a [`Micromux`] instance:
//!
//! ```no_run
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let raw = std::fs::read_to_string("./micromux.yaml")?;
//! let config_dir = std::path::Path::new(".");
//! let file_id = 0usize;
//! let mut diagnostics = vec![];
//! let config = micromux::from_str(&raw, config_dir, file_id, None, &mut diagnostics)?;
//! let mux = micromux::Micromux::new(&config)?;
//! # Ok(()) }
//! ```

mod config;
mod diagnostics;
mod env;
mod graph;
mod health_check;
mod model;
mod scheduler;
mod service;
mod spec;
pub mod structured_log;
#[cfg(test)]
pub(crate) mod test_util;
#[cfg(windows)]
mod windows_job;

use codespan_reporting::{diagnostic::Severity, files::SimpleFiles};
use schemars::JsonSchema;
use serde::Serialize;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

pub use tokio_util::sync::CancellationToken;

pub use config::{
    ConfigError, ConfigFile, ControlConfig, DynamicServicesPolicy, config_file_names,
    find_config_file, from_str, read_config_file, read_config_file_async,
};
pub use diagnostics::{Printer, ToDiagnostics, render_to_string};
pub use env::Error as EnvironmentError;
pub use graph::Error as GraphError;
pub use health_check::Health;
pub use model::{
    ChangeKind, Desired, DiskLogRetention, DynamicServiceInfo, EVENT_HISTORY, Execution,
    HealthAttempt, HealthLine, HealthResult, HealthcheckConfig, LogLimit, LogLine, LogRetention,
    LogRun, LogRunReadError, LogRunSummary, MemoryLogRetention, OriginKind, RestartState,
    RetiredReason, ServiceEvent, ServiceEventKind, ServiceSnapshot, SessionChange,
    SessionModelReader, trim_to_last_bytes,
};
pub use scheduler::{
    Command, CommandRejection, DynamicServiceAck, DynamicServiceResult, MAX_PTY_INPUT_BATCH_BYTES,
    MAX_PTY_PASTE_BYTES, OutputStream, PreparedPtyInput, PtyInputKind, PtyInputPrepareError,
    PtyInputSendError, ReconcileAction, ReconcileActionKind, ReconcileReceipt, ReconcileResult,
    SchedulerStopped, ServiceCommandAck, ServiceCommandResult, ServiceControl, ServiceID,
    TerminalControl,
};
pub use service::{Error as ServiceError, RestartPolicy};
pub use spec::{
    DependencySpec, DynamicOrigin, DynamicServiceParams, HealthcheckSpec, Lease,
    PartialServiceSpec, ServiceOrigin, ServiceSpec, SpecError, SpecField,
};
pub use structured_log::{
    FIELDS_KEY, MESSAGE_KEYS, StructuredLogLevel, find_fields_object, find_key,
    is_structured_log_level_key, key_matches, render_scalar, sanitize_text,
    structured_log_level_in_object, structured_log_level_in_record,
};

pub(crate) type ServiceMap = indexmap::IndexMap<ServiceID, service::Service>;

/// Errors from constructing, validating, or running a micromux session.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Filesystem access failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The canonical configuration path has no parent directory.
    #[error("config path has no parent directory: {}", path.display())]
    MissingConfigParent {
        /// Canonical configuration path.
        path: PathBuf,
    },
    /// Rendering configuration diagnostics failed.
    #[error(transparent)]
    Diagnostics(#[from] codespan_reporting::files::Error),
    /// A service definition could not be materialized.
    #[error(transparent)]
    Service(#[from] ServiceError),
    /// Service dependencies are invalid.
    #[error(transparent)]
    Graph(#[from] GraphError),
}

#[derive(Debug, Clone)]
pub(crate) struct ReloadConfig {
    pub(crate) config_path: PathBuf,
    pub(crate) strict_override: Option<bool>,
}

pub(crate) fn service_map_from_config<F>(
    config_file: &config::ConfigFile<F>,
) -> Result<ServiceMap, ServiceError> {
    let config_dir = config_file.config_dir.clone();
    config_file
        .config
        .services
        .iter()
        .map(|(name, service_config)| {
            let service_id = name.as_ref().clone();
            let service =
                service::Service::new(name.as_ref().clone(), &config_dir, service_config.clone())?;
            Ok::<_, ServiceError>((service_id, service))
        })
        .collect::<Result<ServiceMap, _>>()
}

/// Severity of one config-validation diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfigDiagnosticSeverity {
    /// The config cannot be used.
    Error,
    /// The config is usable but should be corrected.
    Warning,
}

/// One compact config-validation diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ConfigDiagnostic {
    /// Diagnostic severity.
    pub severity: ConfigDiagnosticSeverity,
    /// Human-readable diagnostic message.
    pub message: String,
}

/// Result of running the same config pipeline used to start a session.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ValidationReport {
    /// Whether parsing, normalization, and graph validation all succeeded without errors.
    pub valid: bool,
    /// Canonical config path.
    pub config_path: String,
    /// Normalized service ids in config order.
    pub services: Vec<ServiceID>,
    /// Compact errors and warnings.
    pub diagnostics: Vec<ConfigDiagnostic>,
    /// ANSI-free codespan rendering, capped to 16 KiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rendered: Option<String>,
}

/// Validate a config without constructing or starting a session.
///
/// This runs parsing, service normalization (including environment-file loading and
/// interpolation), and dependency-graph validation. `strict_override` overrides the config's own
/// `strict:` key like the CLI `--strict` flag; pass `None` to let the config decide, so
/// validation agrees with what a session startup would enforce.
///
/// # Errors
///
/// Returns an error when the config path cannot be canonicalized or read.
pub fn validate_config_file(
    path: &std::path::Path,
    strict_override: Option<bool>,
) -> Result<ValidationReport, Error> {
    const MAX_RENDERED: usize = 16 * 1024;

    let config_path = path.canonicalize()?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| Error::MissingConfigParent {
            path: config_path.clone(),
        })?;
    let raw = config::read_config_file(&config_path)?;
    let mut files = SimpleFiles::new();
    let file_id = files.add(config_path.display().to_string(), raw.clone());
    let mut source_diagnostics = Vec::new();
    let mut parsed = match config::from_str(
        &raw,
        config_dir,
        file_id,
        strict_override,
        &mut source_diagnostics,
    ) {
        Ok(config) => Some(config),
        Err(err) => {
            source_diagnostics.extend(err.to_diagnostics(file_id));
            None
        }
    };
    if let Some(config) = &mut parsed {
        config.config_path = Some(config_path.clone());
    }

    let mut services = Vec::new();
    if !source_diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == Severity::Error)
        && let Some(config) = &parsed
    {
        match service_map_from_config(config) {
            Ok(service_map) => {
                services.extend(service_map.keys().cloned());
                if let Err(err) = graph::ServiceGraph::new(&service_map) {
                    source_diagnostics.push(
                        codespan_reporting::diagnostic::Diagnostic::error()
                            .with_message(err.to_string()),
                    );
                }
            }
            Err(err) => source_diagnostics.push(
                codespan_reporting::diagnostic::Diagnostic::error().with_message(err.to_string()),
            ),
        }
    }

    let valid = parsed.is_some()
        && !source_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error);
    let diagnostics = source_diagnostics
        .iter()
        .map(|diagnostic| ConfigDiagnostic {
            severity: match diagnostic.severity {
                Severity::Bug | Severity::Error => ConfigDiagnosticSeverity::Error,
                Severity::Warning | Severity::Note | Severity::Help => {
                    ConfigDiagnosticSeverity::Warning
                }
            },
            message: diagnostic.message.clone(),
        })
        .collect();
    let mut rendered = diagnostics::render_to_string(&files, &source_diagnostics)?;
    if rendered.len() > MAX_RENDERED {
        let mut end = MAX_RENDERED;
        while !rendered.is_char_boundary(end) {
            end -= 1;
        }
        rendered.truncate(end);
    }
    Ok(ValidationReport {
        valid,
        config_path: config_path.display().to_string(),
        services,
        diagnostics,
        rendered: (!rendered.is_empty()).then_some(rendered),
    })
}

pub(crate) fn initial_model_entries(services: &ServiceMap) -> Vec<(ServiceSnapshot, LogRetention)> {
    services
        .iter()
        .map(|(id, service)| {
            let mut snapshot = ServiceSnapshot::initial(
                id.clone(),
                service.display_name().to_string(),
                service.spec.ports.clone(),
                service
                    .spec
                    .healthcheck
                    .as_ref()
                    .map(HealthcheckConfig::from),
                service.spec.restart.clone(),
                service.argv(),
                service.working_dir_display(),
            );
            snapshot.stop_grace_period = service.spec.stop_grace_period;
            snapshot.desired = match service.startup_mode {
                service::StartupMode::Enabled => Desired::Enabled,
                service::StartupMode::Disabled => Desired::Disabled,
            };
            (snapshot, service.log_retention)
        })
        .collect()
}

/// Main entry point to run a micromux session.
pub struct Micromux {
    services: ServiceMap,
    reload_config: Option<ReloadConfig>,
    config_dir: PathBuf,
    dynamic_policy: DynamicServicesPolicy,
    default_log_retention: LogRetention,
}

/// Capability handles returned by [`Micromux::start`].
///
/// The model writer never escapes the core, so the only handles an adapter can hold are the read
/// capability and narrow command capabilities. The policy-enforcing [`ServiceControl`] port has no
/// input forwarding; the trusted in-process TUI also receives [`Handles::terminal`].
pub struct Handles {
    /// Read capability over the session model: query + `subscribe`.
    pub reader: SessionModelReader,
    /// Full trusted in-process lifecycle command sender for the TUI/CLI.
    pub commands: mpsc::Sender<Command>,
    /// Bounded PTY-input and resize capability for the in-process TUI.
    pub terminal: TerminalControl,
    /// Dynamic-service limits latched when the session started.
    pub dynamic_services: DynamicServicesPolicy,
}

impl Handles {
    /// A narrow, untrusted command port for adapters such as the control server and MCP.
    ///
    /// It exposes acknowledged lifecycle operations, including policy-checked dynamic-service
    /// mutations, but cannot forward PTY input or resize terminals.
    #[must_use]
    pub fn service_control(&self) -> ServiceControl {
        ServiceControl::new(self.commands.clone())
    }
}

/// Return the OS-specific project directories for micromux.
///
/// This can be used by frontends (CLI/TUI) to determine where to store log files and other
/// persistent state.
#[must_use]
pub fn project_dir() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("com", "romnn", "micromux")
}

fn memory_retention_is_unbounded(retention: LogRetention) -> bool {
    retention.memory.max_lines == LogLimit::Unbounded
        || retention.memory.max_bytes == LogLimit::Unbounded
}

impl Micromux {
    /// Construct a new [`Micromux`] instance from a parsed [`ConfigFile`].
    ///
    /// # Errors
    ///
    /// Returns an error if a service definition in the configuration cannot be normalized
    /// (e.g. invalid environment interpolation, invalid port parsing, etc.).
    pub fn new(config_file: &config::ConfigFile<diagnostics::FileId>) -> Result<Self, Error> {
        let services = service_map_from_config(config_file)?;
        let unbounded_services = services
            .values()
            .filter(|service| memory_retention_is_unbounded(service.log_retention))
            .count();
        let dynamic_default_unbounded =
            memory_retention_is_unbounded(config_file.config.log_retention);
        if unbounded_services > 0 || dynamic_default_unbounded {
            tracing::warn!(
                configured_services = unbounded_services,
                dynamic_default = dynamic_default_unbounded,
                "in-memory log retention is unbounded"
            );
        }
        let reload_config = config_file
            .config_path
            .clone()
            .map(|config_path| ReloadConfig {
                config_path,
                strict_override: config_file.strict_override,
            });

        graph::ServiceGraph::new(&services)?;

        let mut dynamic_policy = config_file.config.control.dynamic_services.clone();
        for root in &mut dynamic_policy.allowed_working_roots {
            if root.is_relative() {
                *root = config_file.config_dir.join(&*root);
            }
        }

        Ok(Self {
            services,
            reload_config,
            config_dir: config_file.config_dir.clone(),
            dynamic_policy,
            default_log_retention: config_file.config.log_retention,
        })
    }

    /// Start the scheduler, returning the runner future and the capability [`Handles`].
    ///
    /// The model (`Inner` + `Writer`) and the command channel are built internally; the writer is
    /// moved into the runner future and never leaves the core, so adapters can only read the model
    /// or send commands. `Arc<Self>` makes the future `'static`, so the caller can `tokio::spawn` it
    /// while holding the handles.
    pub fn start(
        self: Arc<Self>,
        shutdown: CancellationToken,
    ) -> (impl Future<Output = Result<(), Error>> + 'static, Handles) {
        let (reader, writer) = model::new(initial_model_entries(&self.services));
        let (commands_tx, commands_rx) = mpsc::channel(1024);
        let (terminal, pty_input_rx) = TerminalControl::channel(commands_tx.clone());
        let handles = Handles {
            reader,
            commands: commands_tx,
            terminal,
            dynamic_services: self.dynamic_policy.clone(),
        };

        let runner = async move {
            tracing::info!("starting");
            let (events_tx, events_rx) = mpsc::channel(1024);

            scheduler::scheduler(scheduler::SchedulerInput {
                services: self.services.clone(),
                reload_config: self.reload_config.clone(),
                commands_rx,
                pty_input_rx,
                events_rx,
                events_tx,
                #[cfg(test)]
                test_events_tx: None,
                writer,
                shutdown: shutdown.clone(),
                config_dir: self.config_dir.clone(),
                dynamic_policy: self.dynamic_policy.clone(),
                default_log_retention: self.default_log_retention,
            })
            .await?;
            tracing::info!("exiting");
            Ok::<(), Error>(())
        };

        (runner, handles)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Desired, Execution};
    use color_eyre::eyre;
    use similar_asserts::assert_eq;
    use std::path::Path;

    #[test]
    fn disabled_config_seeds_disabled_model_state() -> eyre::Result<()> {
        let raw = r#"
            version: 1
            services:
              worker:
                command: ["sh", "-c", "sleep 60"]
                disabled: true
        "#;
        let mut diagnostics = Vec::new();
        let config = from_str(raw, Path::new("."), 0usize, None, &mut diagnostics)?;
        let mux = Arc::new(Micromux::new(&config)?);
        let (_runner, handles) = mux.start(CancellationToken::new());
        let snapshot = handles
            .reader
            .service("worker")
            .ok_or_else(|| eyre::eyre!("missing worker snapshot"))?;

        assert!(diagnostics.is_empty());
        assert_eq!(snapshot.desired, Desired::Disabled);
        assert_eq!(snapshot.execution, Execution::Pending);
        assert_eq!(snapshot.run_generation, 0);
        Ok(())
    }

    #[test]
    fn programmatic_dynamic_policy_defaults_resolve_from_the_config_directory() -> eyre::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let raw = "version: 1\nservices: {}\n";
        let mut diagnostics = Vec::new();
        let mut config = from_str(raw, directory.path(), 0usize, None, &mut diagnostics)?;
        config.config.control.dynamic_services = DynamicServicesPolicy::default();

        let mux = Micromux::new(&config)?;

        assert_eq!(
            mux.dynamic_policy.allowed_working_roots,
            vec![directory.path().join(".")]
        );
        Ok(())
    }

    #[test]
    fn config_validation_reports_warnings_and_graph_errors() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("micromux.yaml");
        std::fs::write(
            &config_path,
            r#"version: 1
future_option: true
services:
  api:
    command: ["true"]
    depends_on: [missing]
    future_option: true
"#,
        )?;

        let report = validate_config_file(&config_path, None)?;
        assert!(!report.valid);
        assert_eq!(report.services, vec!["api"]);
        assert!(report.diagnostics.iter().any(|diagnostic| {
            diagnostic.severity == ConfigDiagnosticSeverity::Warning
                && diagnostic.message.contains("unknown service field")
        }));
        assert!(report.diagnostics.iter().any(|diagnostic| {
            diagnostic.severity == ConfigDiagnosticSeverity::Error
                && diagnostic.message.contains("depends on unknown `missing`")
        }));
        assert!(report.rendered.is_some());
        Ok(())
    }

    #[test]
    fn config_validation_honors_the_configs_own_strict_key() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("micromux.yaml");
        std::fs::write(
            &config_path,
            r#"version: 1
strict: true
services:
  api:
    command: ["true"]
    future_option: true
"#,
        )?;

        // With `strict: true` in the config, an unknown key must invalidate the config exactly
        // as it would fail a session startup — `None` lets the config decide.
        let report = validate_config_file(&config_path, None)?;
        assert!(!report.valid);
        assert!(report.diagnostics.iter().any(|diagnostic| {
            diagnostic.severity == ConfigDiagnosticSeverity::Error
                && diagnostic.message.contains("unknown service field")
        }));

        // An explicit override still wins, like the CLI `--strict` flag.
        let relaxed = validate_config_file(&config_path, Some(false))?;
        assert!(relaxed.valid);
        Ok(())
    }
}
