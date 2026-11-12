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

mod cleanup;
mod config;
pub mod containment;
mod diagnostics;
mod env;
mod graph;
mod health_check;
mod model;
#[cfg(unix)]
mod process_tree;
mod scheduler;
mod service;
pub mod session;
mod shell;
mod spec;
pub mod structured_log;
#[cfg(test)]
pub(crate) mod test_util;
#[cfg(windows)]
mod windows_job;

use codespan_reporting::{
    diagnostic::{Diagnostic, Severity},
    files::SimpleFiles,
};
use schemars::JsonSchema;
use serde::Serialize;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

pub use tokio_util::sync::CancellationToken;

pub use cleanup::{
    CleanupWaitError, PreviousCleanupWait, WAITING_FOR_PREVIOUS_CLEANUP, cleanup_directory_in,
    cleanup_state_directory, previous_cleanup_bound, reported_previous_cleanup_waits,
    wait_for_previous_cleanups,
};
pub use config::user::{UserConfig, parse_user_config, user_config_path};
pub use config::{
    ConfigError, ConfigFile, ControlConfig, DependencyCondition, DynamicServicesPolicy,
    MAX_CONFIG_FILE_BYTES, UiConfig, config_file_names, find_config_file, from_str,
    parse_ui_section, read_config_file, read_config_file_async,
};
pub use diagnostics::{Printer, ToDiagnostics, render_to_string};
pub use env::{Error as EnvironmentError, InterpolationError};
pub use graph::Error as GraphError;
pub use health_check::Health;
pub use model::{
    ChangeKind, CleanupStatus, DependencyFailure, DependencyFailureReason, Desired,
    DiskLogRetention, DynamicServiceInfo, EVENT_HISTORY, Execution, HealthAttempt, HealthLine,
    HealthResult, HealthcheckConfig, LogLimit, LogLine, LogRetention, LogRun, LogRunReadError,
    LogRunSummary, MemoryLogRetention, OriginKind, RestartState, RetiredReason, ServiceEvent,
    ServiceEventKind, ServiceSnapshot, SessionChange, SessionModelReader, trim_to_last_bytes,
};
pub use scheduler::{
    Command, CommandRejection, DynamicServiceAck, DynamicServiceResult, EnvironmentResult,
    MAX_PTY_INPUT_BATCH_BYTES, MAX_PTY_PASTE_BYTES, OutputStream, PreparedPtyInput, PtyInputKind,
    PtyInputPrepareError, PtyInputSendError, ReconcileAction, ReconcileActionKind,
    ReconcileReceipt, ReconcileResult, ResolvedEnvironment, SchedulerStopped, ServiceCommandAck,
    ServiceCommandResult, ServiceControl, ServiceID, TerminalControl, TerminalForwarding,
    TerminalSize,
};
pub use service::{
    Error as ServiceError, RestartPolicy, Site as ServiceSite, SkipReason as EnvFileSkipReason,
    SkippedEnvFile, WorkingDirectoryError,
};
pub use session::{
    PortRegistry, Route, RouteError, RoutingStatus, STATE_DIR_VARIABLE, Session,
    SessionEnvironment, SessionError, SessionIdentity, SessionStatus, SessionSummary,
    state_directory_override,
};
pub use shell::shell_join;
pub use spec::{
    DependencySpec, DynamicOrigin, DynamicServiceParams, HealthcheckSpec, Lease,
    PartialServiceSpec, ServiceKind, ServiceOrigin, ServiceSpec, SpecError, SpecField, StopSignal,
};
pub use structured_log::{
    FIELDS_KEY, LogDisplay, MESSAGE_KEYS, RecordTimestamp, StructuredLogLevel, TIMESTAMP_KEYS,
    find_fields_object, find_key, is_structured_log_level_key, is_timestamp_key, key_matches,
    numeric_timestamp_to_unix_ms, render_scalar, sanitize_text, structured_log_level_in_object,
    structured_log_level_in_record, structured_log_timestamp_in_record,
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
    /// One or more service definitions could not be materialized.
    #[error(transparent)]
    Services(#[from] MaterializationFailure),
    /// Service dependencies are invalid.
    #[error(transparent)]
    Graph(#[from] GraphError),
    /// The session's identity or allocated ports could not be established.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// One or more routes could not be resolved.
    #[error(transparent)]
    Routes(#[from] RouteFailure),
}

/// Routes whose ports could not be resolved.
#[derive(Debug, thiserror::Error)]
#[error("{}", self.errors.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
pub struct RouteFailure {
    /// One error per failing route, in config order.
    pub errors: Vec<RouteError>,
}

impl ToDiagnostics for RouteFailure {
    fn to_diagnostics<F: Copy + PartialEq>(&self, file_id: F) -> Vec<Diagnostic<F>> {
        self.errors
            .iter()
            .flat_map(|err| err.to_diagnostics(file_id))
            .collect()
    }
}

/// Where a session re-reads its config from, and how it interprets the file.
#[derive(Debug, Clone)]
pub(crate) struct ReloadConfig {
    pub(crate) config_path: PathBuf,
    /// The directory the session first parsed its config against.
    ///
    /// A reload resolves relative paths against it rather than against the config path's parent.
    /// The two can name one directory differently, such as through a symlinked temp directory,
    /// and every service's working directory is resolved from it, so only the original spelling
    /// makes an unchanged file reload to identical services.
    pub(crate) config_dir: PathBuf,
    pub(crate) strict_override: Option<bool>,
    /// The session latched at startup, which every reload resolves against.
    pub(crate) session: Arc<session::SessionContext>,
}

/// Service definitions that could not be materialized.
///
/// Carries the optional env files skipped while trying, because a skipped file is the usual
/// explanation for a variable that "is in an env file" yet reports as unset.
#[derive(Debug, thiserror::Error)]
#[error("{}", join_messages(errors))]
pub struct MaterializationFailure {
    /// One error per failing service, in config order.
    pub errors: Vec<ServiceError>,
    /// Every optional env file skipped across all services, in config order.
    pub skipped_env_files: Vec<SkippedEnvFile>,
}

fn join_messages(errors: &[ServiceError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

impl ToDiagnostics for MaterializationFailure {
    fn to_diagnostics<F: Copy + PartialEq>(&self, file_id: F) -> Vec<Diagnostic<F>> {
        // Notes first: a skipped file explains the error that follows it.
        self.skipped_env_files
            .iter()
            .flat_map(|skipped| skipped.to_diagnostics(file_id))
            .chain(
                self.errors
                    .iter()
                    .flat_map(|err| err.to_diagnostics(file_id)),
            )
            .collect()
    }
}

/// The services that materialized, plus what went wrong or was skipped for the rest.
struct MaterializedServices {
    services: ServiceMap,
    errors: Vec<ServiceError>,
    skipped_env_files: Vec<SkippedEnvFile>,
}

impl MaterializedServices {
    fn into_result(self) -> Result<ServiceMap, MaterializationFailure> {
        let Self {
            services,
            errors,
            skipped_env_files,
        } = self;
        if errors.is_empty() {
            Ok(services)
        } else {
            Err(MaterializationFailure {
                errors,
                skipped_env_files,
            })
        }
    }
}

/// Materialize every configured service, keeping the errors of the ones that fail.
///
/// Services are independent, so a validation report can name every failing service at once
/// instead of stopping at the first.
fn materialize_services<F>(
    config_file: &config::ConfigFile<F>,
    pinned: &SessionEnvironment,
) -> MaterializedServices {
    let config_dir = &config_file.config_dir;
    // One snapshot of the supervisor environment per config load, so every service resolves
    // against the same values.
    let supervisor_environment: std::collections::HashMap<String, String> =
        std::env::vars().collect();
    let cleanup_directory = config_file
        .config
        .services
        .values()
        .any(|service| service.cleanup.is_some())
        .then(|| cleanup::cleanup_state_directory(cleanup::config_key_path(config_file)))
        .flatten();
    let mut services = ServiceMap::new();
    let mut errors = Vec::new();
    let mut skipped_env_files = Vec::new();
    for (name, service_config) in &config_file.config.services {
        let service_id = name.as_ref().clone();
        match service::Service::from_config(
            service_id.clone(),
            config_dir,
            service_config.clone(),
            &supervisor_environment,
            pinned,
            &mut skipped_env_files,
        ) {
            Ok(mut service) => {
                if let (Some(cleanup), Some(directory)) =
                    (&mut service.spec.cleanup, &cleanup_directory)
                {
                    cleanup.files = Some(cleanup::files_in(directory, &service_id));
                }
                services.insert(service_id, service);
            }
            Err(err) => errors.push(err),
        }
    }
    MaterializedServices {
        services,
        errors,
        skipped_env_files,
    }
}

pub(crate) fn service_map_from_config<F>(
    config_file: &config::ConfigFile<F>,
    pinned: &SessionEnvironment,
) -> Result<ServiceMap, MaterializationFailure> {
    materialize_services(config_file, pinned).into_result()
}

/// Severity of one config-validation diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConfigDiagnosticSeverity {
    /// The config cannot be used.
    Error,
    /// The config is usable but should be corrected.
    Warning,
    /// Informational, such as an optional env file that was skipped.
    Note,
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
        services = validate_materialization(config, file_id, &mut source_diagnostics);
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
                Severity::Warning => ConfigDiagnosticSeverity::Warning,
                Severity::Note | Severity::Help => ConfigDiagnosticSeverity::Note,
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

/// Materializes the services and routes of a parsed config the way a session would, appending
/// what goes wrong to `diagnostics`, and returns the ids of the services that materialized.
///
/// Validation never reserves ports: it shows the persisted assignment, or provisional values for
/// names a session has not been assigned yet or that may move to their preferred port.
fn validate_materialization(
    config: &config::ConfigFile<usize>,
    file_id: usize,
    diagnostics: &mut Vec<Diagnostic<usize>>,
) -> Vec<ServiceID> {
    let session = Session::preview(config, &PortRegistry::user());
    if !session.provisional_ports().is_empty() {
        diagnostics.push(
            codespan_reporting::diagnostic::Diagnostic::note().with_message(format!(
                "no final port is known yet for {}; validated with provisional values, the \
                 session assigns real ones when it starts",
                session.provisional_ports().join(", ")
            )),
        );
    }
    let MaterializedServices {
        services: service_map,
        errors,
        skipped_env_files,
    } = materialize_services(config, &session.environment());
    if errors.is_empty()
        && let Err(route_errors) = session::resolve_routes(
            &session.into_context(),
            &service_map,
            &std::env::vars().collect(),
        )
    {
        for err in &route_errors {
            diagnostics.extend(err.to_diagnostics(file_id));
        }
    }
    for skipped in &skipped_env_files {
        diagnostics.extend(skipped.to_diagnostics(file_id));
    }
    for err in &errors {
        diagnostics.extend(err.to_diagnostics(file_id));
    }
    // The graph is only meaningful once every service materialized: a failed service
    // would otherwise show up as a missing dependency of its dependents.
    if errors.is_empty()
        && let Err(err) = graph::ServiceGraph::new(&service_map)
    {
        diagnostics.push(
            codespan_reporting::diagnostic::Diagnostic::error().with_message(err.to_string()),
        );
    }
    service_map.keys().cloned().collect()
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
                Some(service.working_dir_display()),
            );
            snapshot.stop_grace_period = service.spec.stop_grace_period;
            snapshot.stop_signal = service.spec.stop_signal;
            snapshot.desired = match service.startup_mode {
                service::StartupMode::Enabled | service::StartupMode::Manual => Desired::Enabled,
                service::StartupMode::Disabled => Desired::Disabled,
            };
            snapshot.kind = service.spec.kind;
            snapshot.manual = service.startup_mode == service::StartupMode::Manual;
            snapshot.log_display = service.log_display.clone();
            snapshot.depends_on = service.spec.depends_on.clone();
            (snapshot, service.log_retention)
        })
        .collect()
}

/// Main entry point to run a micromux session.
pub struct Micromux {
    git_metadata_enabled: bool,
    services: ServiceMap,
    session: Arc<session::SessionContext>,
    routes: Vec<Route>,
    reload_config: Option<ReloadConfig>,
    config_dir: PathBuf,
    dynamic_policy: DynamicServicesPolicy,
    default_log_retention: LogRetention,
    default_log_display: LogDisplay,
    /// Where the session keeps its run-log spool; `None` keeps run logs in memory only.
    cache_directory: Option<PathBuf>,
}

/// Capability handles returned by [`Micromux::start`].
///
/// The model writer never escapes the core, so the only handles an adapter can hold are the read
/// capability and narrow command capabilities. The policy-enforcing [`ServiceControl`] port has no
/// input forwarding; the trusted in-process TUI, and the control server on behalf of attached
/// TUIs, also receive [`Handles::terminal`].
pub struct Handles {
    /// Read capability over the session model: query + `subscribe`.
    pub reader: SessionModelReader,
    /// Full trusted in-process lifecycle command sender for the TUI/CLI.
    pub commands: mpsc::Sender<Command>,
    /// Bounded PTY-input and resize capability for the in-process TUI and attached TUIs.
    pub terminal: TerminalControl,
    /// Dynamic-service limits latched when the session started.
    pub dynamic_services: DynamicServicesPolicy,
    /// The session's name, URLs, and allocated ports, with the live routing status.
    pub session: SessionStatus,
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

/// The directory for micromux's own log files and the session run-log spools.
///
/// It is `cache` in the directory [`STATE_DIR_VARIABLE`] names when that is set, and the user's
/// cache directory otherwise.
/// Returns `None` when neither can be determined.
#[must_use]
pub fn cache_directory() -> Option<PathBuf> {
    match state_directory_override() {
        Some(state) => Some(state.join("cache")),
        None => project_dir().map(|directories| directories.cache_dir().to_path_buf()),
    }
}

fn memory_retention_is_unbounded(retention: LogRetention) -> bool {
    retention.memory.max_lines == LogLimit::Unbounded
        || retention.memory.max_bytes == LogLimit::Unbounded
}

impl Micromux {
    /// Construct a new [`Micromux`] instance from a parsed [`ConfigFile`].
    ///
    /// The session is established against the current user's [`PortRegistry`].
    ///
    /// # Errors
    ///
    /// Returns an error if the session cannot be established (see [`Session::establish`]) or a
    /// service definition in the configuration cannot be normalized (e.g. invalid environment
    /// interpolation, invalid port parsing, etc.).
    pub fn new(config_file: &config::ConfigFile<diagnostics::FileId>) -> Result<Self, Error> {
        let session = Session::establish(config_file, &PortRegistry::user())?;
        Self::with_session(config_file, session)
    }

    /// Construct a new [`Micromux`] instance for an already established [`Session`].
    ///
    /// The session is latched: its variables are injected into every service, and a config
    /// reload that changes the session settings is rejected.
    /// Its allocated ports stay reserved until the returned value is dropped.
    ///
    /// # Errors
    ///
    /// Returns an error if a service definition in the configuration cannot be normalized
    /// (e.g. invalid environment interpolation, invalid port parsing, etc.).
    pub fn with_session(
        config_file: &config::ConfigFile<diagnostics::FileId>,
        session: Session,
    ) -> Result<Self, Error> {
        let session = Arc::new(session.into_context());
        let services = service_map_from_config(config_file, &session.environment)?;
        let routes = session::resolve_routes(&session, &services, &std::env::vars().collect())
            .map_err(|errors| RouteFailure { errors })?;
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
                config_dir: config_file.config_dir.clone(),
                strict_override: config_file.strict_override,
                session: session.clone(),
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
            git_metadata_enabled: config_file.config.git_metadata.unwrap_or(true),
            session,
            routes,
            reload_config,
            config_dir: config_file.config_dir.clone(),
            dynamic_policy,
            default_log_retention: config_file.config.log_retention,
            default_log_display: config_file.config.log_display.clone(),
            cache_directory: cache_directory(),
        })
    }

    /// Keeps the session's run-log spool in `directory` instead of [`cache_directory`].
    ///
    /// It serves tests and embedders that keep micromux's files apart from the user's, without
    /// setting [`STATE_DIR_VARIABLE`] for the whole process.
    #[must_use]
    pub fn with_cache_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.cache_directory = Some(directory.into());
        self
    }

    /// The latched session's name and domain.
    #[must_use]
    pub fn session_identity(&self) -> &SessionIdentity {
        &self.session.identity
    }

    /// The session's routes with their ports resolved, in config order, and the owners the config
    /// gave them at startup.
    ///
    /// A running session publishes later owners, after a config reload, through
    /// [`Handles::session`].
    #[must_use]
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }

    /// The session's name, URLs, and allocated ports as latched at startup.
    ///
    /// The routing status starts as [`RoutingStatus::Pending`] for a session with routes; a
    /// running session reports updates through [`Handles::session`].
    #[must_use]
    pub fn session_summary(&self) -> SessionSummary {
        SessionSummary {
            git: None,
            name: self.session.identity.name.clone(),
            domain: self.session.identity.session_domain(),
            host_suffix: self.host_suffix(),
            proxy_port: self.session.proxy_port,
            routes: self.routes.clone(),
            allocated_ports: self.session.ports.clone(),
            routing: (!self.routes.is_empty()).then_some(RoutingStatus::Pending),
        }
    }

    /// The port the shared proxy listens on, as latched when the session was established.
    #[must_use]
    pub fn proxy_port(&self) -> u16 {
        self.session.proxy_port
    }

    /// What follows `<route>.` in this session's browser URLs, such as `luup2.localhost:8080`.
    #[must_use]
    pub fn host_suffix(&self) -> String {
        session::host_suffix(&self.session.identity, self.session.proxy_port)
    }

    /// The port allocated to every `allocate_ports` name, in config order.
    #[must_use]
    pub fn allocated_ports(&self) -> &indexmap::IndexMap<String, u16> {
        &self.session.ports
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
        let (reader, writer) = model::new_in(
            initial_model_entries(&self.services),
            self.cache_directory.as_deref(),
        );
        let (commands_tx, commands_rx) = mpsc::channel(1024);
        let (terminal, pty_input_rx) = TerminalControl::channel(commands_tx.clone());
        let session_status =
            SessionStatus::new(self.session_summary()).with_git_metadata(self.git_metadata_enabled);
        let handles = Handles {
            reader,
            commands: commands_tx,
            terminal,
            dynamic_services: self.dynamic_policy.clone(),
            session: session_status.clone(),
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
                session: self.session.clone(),
                session_status,
                default_log_retention: self.default_log_retention,
                default_log_display: self.default_log_display.clone(),
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

        assert_eq!(diagnostics, Vec::new());
        assert_eq!(snapshot.desired, Desired::Disabled);
        assert_eq!(snapshot.execution, Execution::Pending);
        assert_eq!(snapshot.run_generation, 0);
        Ok(())
    }

    /// Reconciling an unchanged config plans nothing, even when the session parsed it through
    /// another spelling of its directory than the config path records.
    ///
    /// On macOS the temp directory is a symlink into `/private`, so a caller that canonicalizes
    /// only the config path hits exactly this split.
    #[cfg(unix)]
    #[tokio::test]
    async fn reconciling_an_unchanged_config_through_a_symlink_plans_nothing() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let real = directory.path().join("real");
        std::fs::create_dir_all(real.join("work"))?;
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link)?;
        let raw = indoc::indoc! {r#"
            version: 1
            services:
              default-dir:
                command: ["sh", "-c", "sleep 60"]
              own-dir:
                command: ["sh", "-c", "sleep 60"]
                working_dir: work
        "#};
        std::fs::write(real.join("micromux.yaml"), raw)?;

        // Parse through the symlink, but record the config file's canonical path.
        let mut diagnostics = Vec::new();
        let mut config = from_str(raw, &link, 0usize, None, &mut diagnostics)?;
        config.config_path = Some(real.join("micromux.yaml").canonicalize()?);
        let registry = PortRegistry::at(directory.path().join("registry"));
        let mux = Arc::new(Micromux::with_session(
            &config,
            Session::establish(&config, &registry)?,
        )?);
        let shutdown = CancellationToken::new();
        let (runner, handles) = mux.start(shutdown.clone());
        let runner = tokio::spawn(runner);

        let receipt = handles
            .service_control()
            .reconcile_config(true)
            .await?
            .map_err(|rejection| eyre::eyre!("reconcile rejected: {rejection:?}"))?;

        // Neither the default directory nor a relative working_dir reads as a change.
        assert_eq!(receipt.actions, Vec::new());

        shutdown.cancel();
        runner.await??;
        Ok(())
    }

    /// Allocated ports and the session name reach every service, shadow configured values, and
    /// stay latched: a reload that changes the session settings is refused.
    #[cfg(unix)]
    #[tokio::test]
    async fn session_variables_reach_services_and_are_latched() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let project = directory.path().join("My Project");
        std::fs::create_dir_all(&project)?;
        let config_path = project.join("micromux.yaml");
        let raw = indoc::indoc! {r#"
            version: 1
            allocate_ports: [APP_PORT]
            services:
              app:
                command: ["sh", "-c", "sleep 60"]
                environment:
                  APP_PORT: "9540"
                  APP_URL: "http://app.${MICROMUX_SESSION_DOMAIN}:${APP_PORT}"
        "#};
        std::fs::write(&config_path, raw)?;
        let mut diagnostics = Vec::new();
        let mut config = from_str(raw, &project, 0usize, None, &mut diagnostics)?;
        config.config_path = Some(config_path.canonicalize()?);

        let registry = PortRegistry::at(directory.path().join("registry"));
        let session = Session::establish(&config, &registry)?;
        let mux = Arc::new(Micromux::with_session(&config, session)?);

        // The default name is the slugified directory name, and the allocation wins over the
        // inline value.
        assert_eq!(mux.session_identity().name, "my-project");
        let port = mux
            .allocated_ports()
            .get("APP_PORT")
            .copied()
            .ok_or_else(|| eyre::eyre!("APP_PORT was not allocated"))?;
        let environment = &mux
            .services
            .get("app")
            .ok_or_else(|| eyre::eyre!("missing app"))?
            .spec
            .environment;
        assert_eq!(environment.get("APP_PORT"), Some(&port.to_string()));
        assert_eq!(
            environment.get("APP_URL"),
            Some(&format!("http://app.my-project.localhost:{port}"))
        );
        assert_eq!(
            environment.get("MICROMUX_SESSION").map(String::as_str),
            Some("my-project")
        );

        // A reload that adds an allocation is refused instead of half-applied.
        std::fs::write(
            &config_path,
            raw.replace("[APP_PORT]", "[APP_PORT, OTHER_PORT]"),
        )?;
        let shutdown = CancellationToken::new();
        let (runner, handles) = mux.start(shutdown.clone());
        let runner = tokio::spawn(runner);
        let result = handles.service_control().reconcile_config(true).await?;
        let rejection = result.err().map(|rejection| rejection.to_string());
        assert!(
            rejection
                .as_deref()
                .is_some_and(|message| message.contains("restart the session")),
            "{rejection:?}"
        );

        shutdown.cancel();
        runner.await??;
        Ok(())
    }

    /// Every session loaded from a file is registered, so two live sessions never share a name
    /// even when neither allocates ports.
    #[test]
    fn live_sessions_without_allocations_still_need_distinct_names() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path().join("registry"));
        let raw = "version: 1\nsession: { name: qa }\nservices: {}\n";
        let mut configs = Vec::new();
        for project in ["one", "two"] {
            let project = directory.path().join(project);
            std::fs::create_dir_all(&project)?;
            std::fs::write(project.join("micromux.yaml"), raw)?;
            let mut config = from_str(raw, &project, 0usize, None, &mut Vec::new())?;
            config.config_path = Some(project.join("micromux.yaml").canonicalize()?);
            configs.push(config);
        }
        let [one, two] = configs.as_slice() else {
            eyre::bail!("expected two configs");
        };

        let _first = Session::establish(one, &registry)?;
        let second = Session::establish(two, &registry);

        assert!(
            matches!(second, Err(SessionError::NameInUse { .. })),
            "{second:?}"
        );
        Ok(())
    }

    /// Routes resolve against the session variables, name the service listing their port, and
    /// the proxy variables describe the latched proxy port.
    #[cfg(unix)]
    #[test]
    fn routes_resolve_against_the_session_and_its_proxy_port() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let project = directory.path().join("luup2");
        std::fs::create_dir_all(&project)?;
        let raw = indoc::indoc! {r#"
            version: 1
            proxy: { fallback_port: 8080 }
            allocate_ports: [APP_PORT]
            routes:
              - host: app
                port: ${APP_PORT}
              - host: app
                path: /auth/v1/
                strip_prefix: true
                port: 9998
              - host: "*"
                port: 9999
            services:
              web:
                command: ["sh", "-c", "sleep 60"]
                ports: ["${APP_PORT}"]
                environment:
                  APP_URL: "http://app.${MICROMUX_HOST_SUFFIX}"
              auth:
                command: ["sh", "-c", "sleep 60"]
                ports: [9998]
        "#};
        std::fs::write(project.join("micromux.yaml"), raw)?;
        let mut config = from_str(raw, &project, 0usize, None, &mut Vec::new())?;
        config.config_path = Some(project.join("micromux.yaml").canonicalize()?);

        let registry = PortRegistry::at(directory.path().join("registry"));
        let mut session = Session::establish(&config, &registry)?;
        session.set_proxy_port(8080);
        let mux = Micromux::with_session(&config, session)?;

        let port = *mux
            .allocated_ports()
            .get("APP_PORT")
            .ok_or_else(|| eyre::eyre!("APP_PORT was not allocated"))?;
        assert_eq!(mux.host_suffix(), "luup2.localhost:8080");
        assert_eq!(
            mux.routes(),
            &[
                Route {
                    host: "app".to_string(),
                    path: None,
                    strip_prefix: false,
                    url: "http://app.luup2.localhost:8080/".to_string(),
                    port,
                    service: Some("web".to_string()),
                },
                // A path route of the same host has its own URL and owner.
                Route {
                    host: "app".to_string(),
                    path: Some("/auth/v1/".to_string()),
                    strip_prefix: true,
                    url: "http://app.luup2.localhost:8080/auth/v1/".to_string(),
                    port: 9998,
                    service: Some("auth".to_string()),
                },
                Route {
                    host: "*".to_string(),
                    path: None,
                    strip_prefix: false,
                    url: "http://luup2.localhost:8080/".to_string(),
                    port: 9999,
                    service: None,
                },
            ]
        );
        let environment = &mux
            .services
            .get("web")
            .ok_or_else(|| eyre::eyre!("missing web"))?
            .spec
            .environment;
        assert_eq!(
            environment.get("APP_URL").map(String::as_str),
            Some("http://app.luup2.localhost:8080")
        );
        assert_eq!(
            environment.get("MICROMUX_PROXY_PORT").map(String::as_str),
            Some("8080")
        );
        Ok(())
    }

    /// A route pointing back at the proxy, or at a value that is no port, is a config error.
    #[cfg(unix)]
    #[test]
    fn routes_reject_the_proxy_port_and_non_ports() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let raw = indoc::indoc! {r"
            version: 1
            routes:
              - host: loop
                port: 80
              - host: words
                port: http
              - host: unset
                port: ${MICROMUX_TEST_UNSET_ROUTE_PORT}
            services: {}
        "};
        std::fs::write(directory.path().join("micromux.yaml"), raw)?;
        let mut config = from_str(raw, directory.path(), 0usize, None, &mut Vec::new())?;
        config.config_path = Some(directory.path().join("micromux.yaml").canonicalize()?);
        let registry = PortRegistry::at(directory.path().join("registry"));

        let result = Micromux::with_session(&config, Session::establish(&config, &registry)?);

        let Err(Error::Routes(failure)) = result else {
            eyre::bail!("expected route errors");
        };
        let hosts = failure
            .errors
            .iter()
            .map(|err| match err {
                RouteError::ProxyPort { route, .. }
                | RouteError::InvalidPort { route, .. }
                | RouteError::Interpolation { route, .. } => route.as_str(),
            })
            .collect::<Vec<_>>();
        assert_eq!(hosts, vec!["loop", "words", "unset"]);
        assert!(matches!(
            failure.errors.first(),
            Some(RouteError::ProxyPort { port: 80, .. })
        ));
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

    /// Unresolved references are reported for every failing service, name the site and the
    /// variable, and point at the offending value in the source.
    #[test]
    fn config_validation_points_at_unresolved_variables() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("micromux.yaml");
        std::fs::write(
            &config_path,
            r#"version: 1
services:
  in-command:
    command: "echo ${MICROMUX_TEST_UNSET_IN_COMMAND}"
  in-ports:
    command: ["sleep", "1"]
    ports: ["${MICROMUX_TEST_UNSET_IN_PORTS}"]
  in-script:
    command: ["sh", "-c", "for f in *; do echo $f; done"]
    env_file:
      - path: "${MICROMUX_TEST_UNSET_SHARED_ENV}"
        optional: true
  no-dir:
    command: ["true"]
    working_dir: ./definitely-missing
  fine:
    command: ["true"]
"#,
        )?;

        let report = validate_config_file(&config_path, None)?;

        assert!(!report.valid);
        assert_eq!(report.services, vec!["fine"]);
        let messages = report
            .diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity != ConfigDiagnosticSeverity::Warning)
            .map(|diagnostic| (diagnostic.severity, diagnostic.message.as_str()))
            .collect::<Vec<_>>();
        let missing_dir = dir.path().canonicalize()?.join("./definitely-missing");
        // The OS words "not found" differently per platform, so take the text from the same
        // failed lookup instead of spelling it out.
        let not_found = std::fs::metadata(&missing_dir)
            .err()
            .ok_or_else(|| eyre::eyre!("{} unexpectedly exists", missing_dir.display()))?;
        let no_dir_message = format!(
            "cannot use working_dir of service `no-dir`: failed to access working directory {}: \
             {not_found}",
            missing_dir.display()
        );
        assert_eq!(
            messages,
            vec![
                (
                    ConfigDiagnosticSeverity::Note,
                    "optional env_file[0] of service `in-script` skipped: variable \
                     `MICROMUX_TEST_UNSET_SHARED_ENV` is not set",
                ),
                (
                    ConfigDiagnosticSeverity::Error,
                    "cannot resolve command[1] of service `in-command`: variable \
                     `MICROMUX_TEST_UNSET_IN_COMMAND` is not set",
                ),
                (
                    ConfigDiagnosticSeverity::Error,
                    "cannot resolve ports[0] of service `in-ports`: variable \
                     `MICROMUX_TEST_UNSET_IN_PORTS` is not set",
                ),
                (
                    ConfigDiagnosticSeverity::Error,
                    "cannot resolve command[2] of service `in-script`: variable `f` is not set",
                ),
                (ConfigDiagnosticSeverity::Error, no_dir_message.as_str()),
            ]
        );
        let rendered = report
            .rendered
            .ok_or_else(|| eyre::eyre!("missing rendered diagnostics"))?;
        // The rendering quotes the source line and tells the author what to do.
        assert!(
            rendered.contains(r#"ports: ["${MICROMUX_TEST_UNSET_IN_PORTS}"]"#),
            "{rendered}"
        );
        assert!(
            rendered.contains("${MICROMUX_TEST_UNSET_IN_PORTS:-<default>}"),
            "{rendered}"
        );
        // A shell script leads with the literal escape, since a shell-local variable is the
        // likely cause there.
        assert!(
            rendered.contains("write `$$f` to pass a literal `$f` through to the shell"),
            "{rendered}"
        );
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
