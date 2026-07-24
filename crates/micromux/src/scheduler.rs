use crate::{
    DynamicOrigin, DynamicServiceParams, DynamicServicesPolicy, Lease, ReloadConfig, ServiceMap,
    ServiceOrigin, ServiceSpec,
    graph::ServiceGraph,
    health_check::Health,
    model::{
        Desired, DynamicServiceInfo, Execution, HealthcheckConfig, LogRetention, OriginKind,
        RestartState, RetiredReason, ServiceEvent, ServiceEventKind, ServiceSnapshot,
        SessionModelWriter,
    },
    service::{self, Service, StartupMode},
};
use codespan_reporting::diagnostic::Severity;
use color_eyre::eyre;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Initial delay before automatically restarting a service after it exits.
const RESTART_BACKOFF_BASE: Duration = Duration::from_millis(250);
/// Maximum delay the (exponentially doubling) restart backoff grows to.
const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(10);
/// Minimum uptime after which a service is considered stable and its backoff is reset.
const RESTART_BACKOFF_RESET: Duration = RESTART_BACKOFF_MAX;
const MAX_RETIRED_SERVICES: usize = 8;
const IDEMPOTENCY_WINDOW: usize = 64;

#[path = "scheduler/types.rs"]
mod types;
#[cfg(test)]
pub(crate) use types::Event;
pub use types::{Command, OutputStream, ServiceID};
pub(crate) use types::{LogUpdateKind, ProcessEvent, RunId, State};

#[path = "scheduler/control.rs"]
mod control;
pub(crate) use control::CommandAck;
pub use control::{
    CommandRejection, DynamicServiceAck, DynamicServiceResult, ReconcileAction,
    ReconcileActionKind, ReconcileReceipt, ReconcileResult, SchedulerStopped, ServiceCommandAck,
    ServiceCommandResult, ServiceControl,
};

#[path = "scheduler/pty.rs"]
mod pty;

#[path = "scheduler/schedule.rs"]
mod schedule;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DesiredState {
    Enabled,
    Disabled,
}

struct RestartTracker {
    backoff_until: Option<tokio::time::Instant>,
    backoff_delay: Option<Duration>,
    on_failure_max: Option<usize>,
    on_failure_remaining: Option<usize>,
}

impl RestartTracker {
    fn on_failure_max(policy: &service::RestartPolicy) -> Option<usize> {
        match policy {
            service::RestartPolicy::OnFailure {
                max_attempts: Some(max_attempts),
            } => Some(*max_attempts),
            service::RestartPolicy::Always
            | service::RestartPolicy::UnlessStopped
            | service::RestartPolicy::Never
            | service::RestartPolicy::OnFailure { max_attempts: None } => None,
        }
    }

    fn new(policy: &service::RestartPolicy) -> Self {
        let on_failure_max = Self::on_failure_max(policy);

        Self {
            backoff_until: None,
            backoff_delay: None,
            on_failure_max,
            on_failure_remaining: on_failure_max,
        }
    }

    fn reconfigure(&mut self, policy: &service::RestartPolicy) {
        let on_failure_max = Self::on_failure_max(policy);
        if self.on_failure_max != on_failure_max {
            self.on_failure_max = on_failure_max;
            self.on_failure_remaining = on_failure_max;
        }
    }

    fn clear_backoff(&mut self) {
        self.backoff_until = None;
        self.backoff_delay = None;
    }

    fn reset_failure_budget(&mut self) {
        self.on_failure_remaining = self.on_failure_max;
    }

    fn apply_backoff(&mut self, stable: bool) {
        if stable {
            self.backoff_delay = None;
            self.reset_failure_budget();
        }

        let next = self.backoff_delay.map_or(RESTART_BACKOFF_BASE, |delay| {
            (delay * 2).min(RESTART_BACKOFF_MAX)
        });
        self.backoff_delay = Some(next);
        self.backoff_until = Some(tokio::time::Instant::now() + next);
    }

    fn remaining_failure_restarts(&self, policy: &service::RestartPolicy) -> Option<usize> {
        match policy {
            service::RestartPolicy::OnFailure {
                max_attempts: Some(max_attempts),
            } => Some(self.on_failure_remaining.unwrap_or(*max_attempts)),
            service::RestartPolicy::Always
            | service::RestartPolicy::UnlessStopped
            | service::RestartPolicy::Never
            | service::RestartPolicy::OnFailure { max_attempts: None } => None,
        }
    }

    fn decrement_failure_restart(&mut self, policy: &service::RestartPolicy) {
        if let service::RestartPolicy::OnFailure {
            max_attempts: Some(max_attempts),
        } = policy
        {
            let remaining = self.on_failure_remaining.get_or_insert(*max_attempts);
            *remaining = remaining.saturating_sub(1);
        }
    }

    /// The currently active backoff as snapshot data, or `None` once the deadline has passed (the
    /// retained `backoff_delay` then only seeds the *next* doubling and is not an active delay).
    fn active_restart_state(&self, policy: &service::RestartPolicy) -> Option<RestartState> {
        match (self.backoff_until, self.backoff_delay) {
            (Some(deadline), Some(backoff_delay)) if deadline > tokio::time::Instant::now() => {
                Some(RestartState {
                    backoff_delay,
                    restarts_remaining: self.remaining_failure_restarts(policy),
                })
            }
            _ => None,
        }
    }
}

pub(super) struct RunningService {
    run_id: RunId,
    pid: Option<u32>,
    terminate: CancellationToken,
    log_reader: Option<pty::LogReaderHandle>,
    pty: pty::PtyHandles,
    since: tokio::time::Instant,
}

impl RunningService {
    fn cancel(&mut self) {
        self.terminate.cancel();
        if let Some(log_reader) = &mut self.log_reader {
            log_reader.cancel();
        }
    }

    fn stable(&self) -> bool {
        self.since.elapsed() >= RESTART_BACKOFF_RESET
    }

    fn finish(mut self) -> (RunId, bool, Option<pty::LogReaderHandle>) {
        let run_id = self.run_id;
        let stable = self.stable();
        let log_reader = self.log_reader.take();
        (run_id, stable, log_reader)
    }
}

impl Drop for RunningService {
    fn drop(&mut self) {
        self.terminate.cancel();
        if let Some(log_reader) = &mut self.log_reader {
            log_reader.cancel();
        }
    }
}

/// Presentation fields captured at spawn time, so config reloads cannot rewrite what an already
/// running or exited run actually executed.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct RunConfig {
    command: Vec<String>,
    working_dir: Option<String>,
    advertised_ports: Vec<u16>,
    healthcheck: Option<HealthcheckConfig>,
}

impl From<&Service> for RunConfig {
    fn from(service: &Service) -> Self {
        Self {
            command: service.argv(),
            working_dir: service.working_dir_display(),
            advertised_ports: service.spec.ports.clone(),
            healthcheck: service
                .spec
                .healthcheck
                .as_ref()
                .map(HealthcheckConfig::from),
        }
    }
}

pub(super) struct ServiceRuntime {
    desired: DesiredState,
    start_requested: bool,
    clear_logs_on_start: bool,
    next_run_id: u64,
    running: Option<RunningService>,
    restart: RestartTracker,
    state: State,
    /// The id of the most recently started run, retained after `running` is cleared so an exited or
    /// disabled service can still report the generation that just ran (for `wait_for_healthy`).
    last_run_id: Option<RunId>,
    /// Monotonic start instant used to compute live uptime. `Some` iff running.
    uptime_started_at: Option<std::time::Instant>,
    /// Wall-clock start time (Unix milliseconds) used only for operator-facing runtime identity.
    /// `Some` iff running.
    started_at_unix_ms: Option<u64>,
    /// Exit code of the most recently finished run.
    last_exit_code: Option<i32>,
    /// Fields that describe the most recent run rather than the current config.
    run_config: Option<RunConfig>,
    draining_log_readers: Vec<(RunId, pty::LogReaderHandle)>,
    retired: Option<RetiredReason>,
    retired_at_unix_ms: Option<u64>,
    expires_at: Option<tokio::time::Instant>,
    last_blocked_on: Vec<ServiceID>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RosterEntryState {
    LiveConfigured,
    RetiredConfigured,
    LiveDynamic,
    RetiredDynamic,
}

fn roster_entry_state(
    runtimes: &HashMap<ServiceID, ServiceRuntime>,
    service_id: &ServiceID,
    service: &Service,
) -> Option<RosterEntryState> {
    let retired = runtimes.get(service_id)?.is_retired();
    Some(match (&service.origin, retired) {
        (ServiceOrigin::Configured, false) => RosterEntryState::LiveConfigured,
        (ServiceOrigin::Configured, true) => RosterEntryState::RetiredConfigured,
        (ServiceOrigin::Dynamic(_), false) => RosterEntryState::LiveDynamic,
        (ServiceOrigin::Dynamic(_), true) => RosterEntryState::RetiredDynamic,
    })
}

#[derive(Clone, Copy)]
struct ServiceRuntimeInit<'a> {
    restart_policy: &'a service::RestartPolicy,
    startup_mode: StartupMode,
}

impl<'a> From<&'a Service> for ServiceRuntimeInit<'a> {
    fn from(service: &'a Service) -> Self {
        Self {
            restart_policy: &service.spec.restart,
            startup_mode: service.startup_mode,
        }
    }
}

impl ServiceRuntime {
    fn new(init: ServiceRuntimeInit<'_>) -> Self {
        let desired = match init.startup_mode {
            StartupMode::Enabled => DesiredState::Enabled,
            StartupMode::Disabled => DesiredState::Disabled,
        };
        let state = match desired {
            DesiredState::Enabled => State::Pending,
            DesiredState::Disabled => State::Disabled,
        };

        Self {
            desired,
            start_requested: false,
            clear_logs_on_start: false,
            next_run_id: 0,
            running: None,
            restart: RestartTracker::new(init.restart_policy),
            state,
            last_run_id: None,
            uptime_started_at: None,
            started_at_unix_ms: None,
            last_exit_code: None,
            run_config: None,
            draining_log_readers: Vec::new(),
            retired: None,
            retired_at_unix_ms: None,
            expires_at: None,
            last_blocked_on: Vec::new(),
        }
    }

    fn reconfigure(&mut self, policy: &service::RestartPolicy) {
        self.restart.reconfigure(policy);
    }

    fn is_live(&self) -> bool {
        self.retired.is_none()
    }

    fn is_retired(&self) -> bool {
        self.retired.is_some()
    }

    fn current_run_id(&self) -> Option<RunId> {
        self.running.as_ref().map(|running| running.run_id)
    }

    /// The public run generation: the current run's id if running, else the last run's id, else 0
    /// (never started).
    fn run_generation(&self) -> u64 {
        self.running
            .as_ref()
            .map(|running| running.run_id)
            .or(self.last_run_id)
            .map_or(0, RunId::get)
    }

    /// Mark the service started: record the live run handle and the start instant. Called from the
    /// schedule path right after a successful spawn.
    fn mark_started(&mut self, running: RunningService) {
        self.uptime_started_at = Some(std::time::Instant::now());
        self.started_at_unix_ms = unix_now_ms();
        self.running = Some(running);
        self.state = State::Running { health: None };
    }

    /// Mark a spawn attempt in progress.
    fn mark_starting(&mut self) {
        self.state = State::Starting;
    }

    /// Update the cached health from a resolved probe, but only while a process is live.
    ///
    /// Returns whether the state actually changed, so callers can distinguish a real health
    /// transition from a probe result that landed after the run was cancelled (e.g. `Killed`).
    fn mark_health(&mut self, health: Health) -> bool {
        if matches!(self.state, State::Running { health: Some(current) } if current == health) {
            return false;
        }
        if matches!(self.state, State::Running { .. } | State::Starting) {
            self.state = State::Running {
                health: Some(health),
            };
            true
        } else {
            false
        }
    }

    /// Note that the running process was signalled (kill in flight).
    fn mark_killed(&mut self) {
        if self.desired == DesiredState::Enabled {
            self.state = State::Killed;
        }
    }

    fn allocate_run_id(&mut self) -> RunId {
        self.next_run_id = self.next_run_id.checked_add(1).unwrap_or(1);
        RunId::new(self.next_run_id)
    }

    fn request_restart(&mut self) {
        self.desired = DesiredState::Enabled;
        self.start_requested = true;
        self.clear_logs_on_start = true;
        self.restart.clear_backoff();
        self.restart.reset_failure_budget();
        if matches!(self.state, State::Disabled) && self.running.is_none() {
            self.state = State::Pending;
        }
        if let Some(running) = &mut self.running {
            running.cancel();
            self.state = State::Killed;
        }
    }

    fn request_enable(&mut self) {
        self.desired = DesiredState::Enabled;
        self.restart.clear_backoff();
        self.restart.reset_failure_budget();

        if self.running.is_some() && !matches!(self.state, State::Disabled | State::Killed) {
            return;
        }

        self.start_requested = true;
        self.clear_logs_on_start = false;
        if matches!(self.state, State::Disabled) && self.running.is_none() {
            self.state = State::Pending;
        }
        if let Some(running) = &mut self.running {
            running.cancel();
            self.state = State::Killed;
        }
    }

    fn disable(&mut self) {
        self.desired = DesiredState::Disabled;
        self.start_requested = false;
        self.clear_logs_on_start = false;
        self.restart.clear_backoff();
        self.state = State::Disabled;
        if let Some(running) = &mut self.running {
            running.cancel();
        }
    }

    fn finish_current_run(&mut self, policy: &service::RestartPolicy, exit_code: i32) {
        // Preserve the generation and exit code before dropping the run handle so an exited or
        // disabled service can still be projected with an accurate `run_generation`/`last_exit_code`.
        let Some(running) = self.running.take() else {
            self.finish_run_state(policy, exit_code, None, false);
            return;
        };
        let (finished_run_id, stable, log_reader) = running.finish();
        if let Some(log_reader) = log_reader {
            self.draining_log_readers
                .push((finished_run_id, log_reader));
        }
        self.finish_run_state(policy, exit_code, Some(finished_run_id), stable);
    }

    /// Drop the reader handle for a run whose `LogReaderFinished` event was just processed. The
    /// reader can finish either before or after the run's `Exited` event: afterwards the handle
    /// sits in `draining_log_readers`; beforehand it is still attached to the live run and must be
    /// cleared here so `finish_current_run` does not park an already-finished reader in the
    /// draining list, where nothing would ever remove it (its finish event has been consumed).
    fn finish_log_reader(&mut self, run_id: RunId) {
        if let Some(running) = &mut self.running
            && running.run_id == run_id
        {
            running.log_reader = None;
        }
        if let Some(idx) = self
            .draining_log_readers
            .iter()
            .position(|(draining_run_id, _)| *draining_run_id == run_id)
        {
            self.draining_log_readers.remove(idx);
        }
    }

    fn finish_failed_start(
        &mut self,
        policy: &service::RestartPolicy,
        run_id: RunId,
        exit_code: i32,
    ) {
        self.finish_run_state(policy, exit_code, Some(run_id), false);
    }

    fn finish_run_state(
        &mut self,
        policy: &service::RestartPolicy,
        exit_code: i32,
        finished_run_id: Option<RunId>,
        stable: bool,
    ) {
        if let Some(run_id) = finished_run_id {
            self.last_run_id = Some(run_id);
        }
        self.uptime_started_at = None;
        self.started_at_unix_ms = None;
        self.last_exit_code = Some(exit_code);
        if self.desired == DesiredState::Disabled {
            self.state = State::Disabled;
            self.restart.clear_backoff();
        } else {
            self.state = State::Exited { exit_code };
            if self.start_requested {
                self.restart.clear_backoff();
            } else if self.will_auto_restart(policy, exit_code) {
                self.restart.apply_backoff(stable);
            } else {
                self.restart.clear_backoff();
            }
        }
    }

    fn will_auto_restart(&self, policy: &service::RestartPolicy, exit_code: i32) -> bool {
        match policy {
            service::RestartPolicy::Always | service::RestartPolicy::UnlessStopped => true,
            service::RestartPolicy::Never => false,
            service::RestartPolicy::OnFailure { max_attempts } => {
                if exit_code == 0 {
                    false
                } else if max_attempts.is_none() {
                    true
                } else {
                    self.restart
                        .remaining_failure_restarts(policy)
                        .is_some_and(|remaining| remaining > 0)
                }
            }
        }
    }
}

/// Project a service's runtime state through the desired/execution table into a wire snapshot. Pure
/// (no lock, no writer) so it can be unit-tested in isolation. `uptime` is left `None` here and
/// refreshed at read time by the model from the returned start instant.
pub(super) fn project_snapshot(
    service: &Service,
    runtime: &ServiceRuntime,
) -> (ServiceSnapshot, Option<std::time::Instant>) {
    let running = runtime.running.is_some();
    let ran_before = runtime.last_run_id.is_some();
    let execution = project_execution(running, &runtime.state, ran_before);
    let health = match (execution, &runtime.state) {
        (Execution::Running, State::Running { health }) => *health,
        _ => None,
    };
    let current_config = RunConfig::from(service);
    let run_config = runtime.run_config.as_ref().unwrap_or(&current_config);
    let restart_state = runtime.restart.active_restart_state(&service.spec.restart);
    let snapshot = ServiceSnapshot {
        id: service.id.clone(),
        name: service.display_name().to_string(),
        origin: match service.origin {
            ServiceOrigin::Configured => OriginKind::Configured,
            ServiceOrigin::Dynamic(_) => OriginKind::Dynamic,
        },
        dynamic: match &service.origin {
            ServiceOrigin::Configured => None,
            ServiceOrigin::Dynamic(origin) => Some(DynamicServiceInfo {
                created_at_unix_ms: origin.created_at_unix_ms,
                expires_at_unix_ms: origin.expires_at_unix_ms,
                owner: origin.owner.clone(),
                revision: origin.revision,
            }),
        },
        retired: runtime.retired,
        retired_at_unix_ms: runtime.retired_at_unix_ms,
        desired: match runtime.desired {
            DesiredState::Enabled => Desired::Enabled,
            DesiredState::Disabled => Desired::Disabled,
        },
        execution,
        health,
        run_generation: runtime.run_generation(),
        pid: runtime.running.as_ref().and_then(|running| running.pid),
        started_at_unix_ms: runtime.started_at_unix_ms,
        advertised_ports: run_config.advertised_ports.clone(),
        healthcheck_configured: run_config.healthcheck.is_some(),
        healthcheck: run_config.healthcheck.clone(),
        config_stale: runtime
            .run_config
            .as_ref()
            .is_some_and(|captured| captured != &current_config),
        restart_state,
        last_exit_code: runtime.last_exit_code,
        command: run_config.command.clone(),
        working_dir: run_config.working_dir.clone(),
        uptime: None,
        restart_policy: service.spec.restart.clone(),
    };
    (snapshot, runtime.uptime_started_at)
}

/// The lease a dynamic service actually gets, from the request and the session cap. Pure so the
/// clamp table is unit-testable: omitted or `"none"` fall to the policy default (the cap itself,
/// or unbounded when the policy is uncapped); a bounded request is clamped to the cap.
fn effective_lifetime(
    max_lifetime: Option<Duration>,
    requested: Option<Lease>,
) -> Option<Duration> {
    match (requested, max_lifetime) {
        (None | Some(Lease::Unbounded), maximum) => maximum,
        (Some(Lease::After(requested)), Some(maximum)) => Some(requested.min(maximum)),
        (Some(Lease::After(requested)), None) => Some(requested),
    }
}

/// Current wall-clock time in Unix milliseconds, matching the unix-ms convention of log-entry
/// timestamps so a run's start can be correlated with log `since` filters. `None` if the clock
/// reads before the epoch or overflows `u64` (never in practice).
fn unix_now_ms() -> Option<u64> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    u64::try_from(elapsed.as_millis()).ok()
}

fn service_event(
    run_generation: u64,
    kind: ServiceEventKind,
    detail: impl Into<String>,
) -> ServiceEvent {
    ServiceEvent {
        seq: 0,
        at_unix_ms: unix_now_ms().unwrap_or_default(),
        run_generation,
        kind,
        detail: detail.into(),
        exit_code: None,
        pid: None,
        delay_ms: None,
        blocked_on: None,
    }
}

/// The decisive desired/execution mapping. The notable row is *running + Disabled → Stopping*: a
/// disabled service that is still draining is never reported as already-Exited.
fn project_execution(running: bool, state: &State, ran_before: bool) -> Execution {
    if running {
        match state {
            State::Running { .. } => Execution::Running,
            // A process is live but a stop/restart is in flight (Killed), or it is draining after a
            // disable (Disabled). Either way: Stopping.
            State::Killed | State::Disabled => Execution::Stopping,
            // Starting, or a transient where a run handle exists while the state still reads
            // Pending/Exited.
            State::Starting | State::Pending | State::Exited { .. } => Execution::Starting,
        }
    } else {
        match state {
            State::Pending => Execution::Pending,
            State::Starting | State::Running { .. } => Execution::Starting,
            State::Killed => Execution::Stopping,
            State::Exited { .. } => Execution::Exited,
            State::Disabled => {
                if ran_before {
                    Execution::Exited
                } else {
                    Execution::Pending
                }
            }
        }
    }
}

fn load_services_from_disk(reload: &ReloadConfig) -> Result<ServiceMap, String> {
    let raw = std::fs::read_to_string(&reload.config_path)
        .map_err(|err| format!("read {}: {err}", reload.config_path.display()))?;
    let config_dir = reload
        .config_path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", reload.config_path.display()))?;
    let mut diagnostics = Vec::new();
    let config = crate::config::from_str(
        &raw,
        config_dir,
        0usize,
        reload.strict_override,
        &mut diagnostics,
    )
    .map_err(|err| format!("parse {}: {err}", reload.config_path.display()))?;
    let errors = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .map(|diagnostic| diagnostic.message.clone())
        .collect::<Vec<_>>();
    if !errors.is_empty() {
        return Err(format!(
            "parse {}: {}",
            reload.config_path.display(),
            errors.join("; ")
        ));
    }
    let services = crate::service_map_from_config(&config)
        .map_err(|err| format!("normalize {}: {err}", reload.config_path.display()))?;
    ServiceGraph::new(&services)
        .map_err(|err| format!("validate {}: {err}", reload.config_path.display()))?;
    Ok(services)
}

fn validate_reloaded_services(
    current: &ServiceMap,
    runtimes: &HashMap<ServiceID, ServiceRuntime>,
    updated: &ServiceMap,
) -> Result<(), String> {
    if let Some(id) = updated.keys().find(|id| {
        current
            .get(*id)
            .is_some_and(|service| matches!(service.origin, ServiceOrigin::Dynamic(_)))
    }) {
        return Err(format!(
            "`{id}` exists as a dynamic service in this session; rename it or restart the session to discard dynamic entries"
        ));
    }
    let configured = current
        .iter()
        .filter_map(|(id, service)| {
            (roster_entry_state(runtimes, id, service) == Some(RosterEntryState::LiveConfigured))
                .then_some(id)
        })
        .collect::<std::collections::HashSet<_>>();
    let missing = current
        .iter()
        .filter(|(id, service)| {
            roster_entry_state(runtimes, id, service) == Some(RosterEntryState::LiveConfigured)
        })
        .map(|(id, _)| id)
        .filter(|service_id| !updated.contains_key(*service_id))
        .map(|id| (*id).clone())
        .collect::<Vec<_>>();
    let added = updated
        .keys()
        .filter(|service_id| !configured.contains(*service_id))
        .cloned()
        .collect::<Vec<_>>();

    match (missing.is_empty(), added.is_empty()) {
        (true, true) => Ok(()),
        (false, true) => Err(format!(
            "config reload cannot remove services while the session is running: {}",
            missing.join(", ")
        )),
        (true, false) => Err(format!(
            "config reload cannot add services while the session is running; use reconcile_config: {}",
            added.join(", ")
        )),
        (false, false) => Err(format!(
            "config reload cannot add/remove services while the session is running; use reconcile_config; removed: {}; added: {}",
            missing.join(", "),
            added.join(", ")
        )),
    }
}

fn sync_model(writer: &SessionModelWriter, service: &Service, runtime: &ServiceRuntime) {
    let (snapshot, started_at) = project_snapshot(service, runtime);
    writer.write_snapshot(snapshot, started_at);
}

#[cfg(test)]
struct TestEventSink {
    tx: mpsc::Sender<Event>,
}

#[cfg(test)]
impl TestEventSink {
    fn new(tx: mpsc::Sender<Event>) -> Self {
        Self { tx }
    }

    fn forward(&self, event: Event) {
        let _ = self.tx.try_send(event);
    }
}

struct SchedulerRuntime {
    services: HashMap<ServiceID, ServiceRuntime>,
    reload_config: Option<ReloadConfig>,
    current_pty_size: portable_pty::PtySize,
    events_tx: mpsc::Sender<ProcessEvent>,
    #[cfg(test)]
    test_events: TestEventSink,
    writer: SessionModelWriter,
    shutdown: CancellationToken,
    config_dir: PathBuf,
    dynamic_policy: DynamicServicesPolicy,
    default_log_retention: LogRetention,
    idempotency: VecDeque<IdempotencyRecord>,
}

struct SchedulerResources {
    reload_config: Option<ReloadConfig>,
    events_tx: mpsc::Sender<ProcessEvent>,
    #[cfg(test)]
    test_events: TestEventSink,
    writer: SessionModelWriter,
    shutdown: CancellationToken,
    config_dir: PathBuf,
    dynamic_policy: DynamicServicesPolicy,
    default_log_retention: LogRetention,
}

#[derive(Clone)]
struct IdempotencyRecord {
    key: String,
    digest: u64,
    service: ServiceID,
    revision: u64,
    ack: DynamicServiceAck,
}

impl SchedulerRuntime {
    fn new(services: &ServiceMap, resources: SchedulerResources) -> Self {
        let services = services
            .iter()
            .map(|(service_id, service)| {
                (
                    service_id.clone(),
                    ServiceRuntime::new(ServiceRuntimeInit::from(service)),
                )
            })
            .collect();
        let SchedulerResources {
            reload_config,
            events_tx,
            #[cfg(test)]
            test_events,
            writer,
            shutdown,
            config_dir,
            dynamic_policy,
            default_log_retention,
        } = resources;

        Self {
            services,
            reload_config,
            current_pty_size: portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            events_tx,
            #[cfg(test)]
            test_events,
            writer,
            shutdown,
            config_dir,
            dynamic_policy,
            default_log_retention,
            idempotency: VecDeque::new(),
        }
    }

    /// Re-project a single service into the model. Both shared borrows (`services` meta and the
    /// runtime map) are disjoint from the writer field, so this never holds a lock across an await.
    fn sync(&self, services: &ServiceMap, service_id: &ServiceID) {
        if let (Some(service), Some(runtime)) =
            (services.get(service_id), self.services.get(service_id))
        {
            sync_model(&self.writer, service, runtime);
        }
    }

    fn sync_all(&self, services: &ServiceMap) {
        for service_id in services.keys() {
            self.sync(services, service_id);
        }
    }

    fn append_event(
        &self,
        service_id: &ServiceID,
        kind: ServiceEventKind,
        detail: impl Into<String>,
    ) {
        let generation = self
            .services
            .get(service_id)
            .map_or(0, ServiceRuntime::run_generation);
        self.writer
            .append_event(service_id, service_event(generation, kind, detail));
    }

    fn require_dynamic_enabled(&self) -> Result<(), CommandRejection> {
        if self.dynamic_policy.enabled {
            Ok(())
        } else {
            Err(CommandRejection::PolicyDenied(
                "set control.dynamic_services.enabled: true in micromux.yaml and restart the session"
                    .to_string(),
            ))
        }
    }

    /// Actionable guidance for a command that hit a retired service, matched to how it retired:
    /// a reconcile-removed configured service revives through the config file, everything else
    /// through `replace_dynamic_service`.
    fn retired_guidance(reason: RetiredReason) -> &'static str {
        match reason {
            RetiredReason::Removed => {
                "the service was removed from micromux.yaml; re-add it and run reconcile_config to revive it"
            }
            RetiredReason::Stopped | RetiredReason::Expired | RetiredReason::Unknown => {
                "the dynamic service is retired; use replace_dynamic_service to revive it"
            }
        }
    }

    fn validate_service_id(id: &str) -> Result<(), CommandRejection> {
        if (1..=64).contains(&id.len())
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            Ok(())
        } else {
            Err(CommandRejection::InvalidSpec(format!(
                "service id `{id}` must match [A-Za-z0-9._-]{{1,64}}"
            )))
        }
    }

    fn request_digest(
        operation: &str,
        expected_revision: Option<u64>,
        params: &DynamicServiceParams,
    ) -> Result<u64, CommandRejection> {
        let bytes = serde_json::to_vec(&(operation, expected_revision, params)).map_err(|err| {
            CommandRejection::InvalidSpec(format!("failed to encode request: {err}"))
        })?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut hasher);
        Ok(hasher.finish())
    }

    fn replay_idempotent(
        &self,
        key: Option<&str>,
        digest: u64,
        services: &ServiceMap,
    ) -> Result<Option<DynamicServiceAck>, CommandRejection> {
        let Some(key) = key else {
            return Ok(None);
        };
        let Some(record) = self.idempotency.iter().find(|record| record.key == key) else {
            return Ok(None);
        };
        if record.digest != digest {
            return Err(CommandRejection::PolicyDenied(
                "idempotency key reused with a different request".to_string(),
            ));
        }
        let revision_matches =
            services
                .get(&record.service)
                .and_then(|service| match &service.origin {
                    ServiceOrigin::Configured => None,
                    ServiceOrigin::Dynamic(origin) => Some(origin.revision),
                })
                == Some(record.revision);
        if !revision_matches {
            return Ok(None);
        }
        let mut ack = record.ack.clone();
        ack.idempotent_replay = true;
        Ok(Some(ack))
    }

    fn remember_idempotent(&mut self, key: Option<String>, digest: u64, ack: &DynamicServiceAck) {
        let Some(key) = key else {
            return;
        };
        self.idempotency.retain(|record| record.key != key);
        while self.idempotency.len() >= IDEMPOTENCY_WINDOW {
            self.idempotency.pop_front();
        }
        self.idempotency.push_back(IdempotencyRecord {
            key,
            digest,
            service: ack.service.clone(),
            revision: ack.revision,
            ack: ack.clone(),
        });
    }

    fn effective_lifetime(&self, requested: Option<Lease>) -> Option<Duration> {
        effective_lifetime(self.dynamic_policy.max_lifetime, requested)
    }

    fn expiry_deadline(lifetime: Duration) -> Result<tokio::time::Instant, CommandRejection> {
        tokio::time::Instant::now()
            .checked_add(lifetime)
            .ok_or_else(|| {
                CommandRejection::InvalidSpec(
                    "effective expires_after is too large for the monotonic clock".to_string(),
                )
            })
    }

    fn lease_deadlines(
        lifetime: Option<Duration>,
    ) -> Result<(Option<tokio::time::Instant>, Option<u64>), CommandRejection> {
        let expires_at = lifetime.map(Self::expiry_deadline).transpose()?;
        let now = unix_now_ms().unwrap_or_default();
        let expires_at_unix_ms = lifetime.map(|lifetime| {
            let lifetime_ms = u64::try_from(lifetime.as_millis()).unwrap_or(u64::MAX);
            now.saturating_add(lifetime_ms)
        });
        Ok((expires_at, expires_at_unix_ms))
    }

    fn lease_detail(expires_at_unix_ms: Option<u64>) -> String {
        expires_at_unix_ms.map_or_else(
            || "lease is unbounded".to_string(),
            |expires_at| format!("lease expires at {expires_at}"),
        )
    }

    fn materialize_spec(
        &self,
        services: &ServiceMap,
        params: &DynamicServiceParams,
    ) -> Result<(ServiceSpec, Option<Duration>), CommandRejection> {
        let base = if let Some(from_service) = &params.from_service {
            services
                .get(from_service)
                .map(|service| service.spec.clone())
                .ok_or_else(|| {
                    CommandRejection::InvalidSpec(format!(
                        "from_service `{from_service}` does not exist"
                    ))
                })?
        } else {
            ServiceSpec::default()
        };
        let mut spec = params.spec.clone().apply_to(base);
        spec.normalize()
            .map_err(|err| CommandRejection::InvalidSpec(err.to_string()))?;
        spec.command.extend(params.extra_args.clone());

        let working_dir = spec
            .working_dir
            .clone()
            .unwrap_or_else(|| self.config_dir.clone());
        let working_dir = if working_dir.is_absolute() {
            working_dir
        } else {
            self.config_dir.join(working_dir)
        };
        let working_dir = std::fs::canonicalize(&working_dir).map_err(|err| {
            CommandRejection::InvalidSpec(format!(
                "working directory `{}` cannot be resolved: {err}",
                working_dir.display()
            ))
        })?;
        if !self
            .dynamic_policy
            .allowed_working_roots
            .iter()
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .any(|root| working_dir.starts_with(root))
        {
            return Err(CommandRejection::PolicyDenied(format!(
                "working directory `{}` is outside control.dynamic_services.allowed_working_roots",
                working_dir.display()
            )));
        }
        spec.working_dir = Some(working_dir);
        Ok((spec, self.effective_lifetime(params.expires_after)))
    }

    fn validate_candidate(
        &self,
        services: &ServiceMap,
        candidate: &Service,
    ) -> Result<(), CommandRejection> {
        for dependency in &candidate.spec.depends_on {
            if dependency.service == candidate.id {
                return Err(CommandRejection::InvalidSpec(format!(
                    "service `{}` cannot depend on itself",
                    candidate.id
                )));
            }
            let Some(runtime) = self.services.get(&dependency.service) else {
                return Err(CommandRejection::InvalidSpec(format!(
                    "service `{}` depends on unknown `{}`",
                    candidate.id, dependency.service
                )));
            };
            if let Some(reason) = runtime.retired {
                return Err(CommandRejection::InvalidSpec(format!(
                    "service `{}` depends on retired service `{}`; {}",
                    candidate.id,
                    dependency.service,
                    Self::retired_guidance(reason)
                )));
            }
        }
        let mut union = services.clone();
        union.insert(candidate.id.clone(), candidate.clone());
        ServiceGraph::new(&union).map_err(|err| CommandRejection::InvalidSpec(err.to_string()))?;
        Ok(())
    }

    fn dynamic_origin(service: &Service) -> Result<&DynamicOrigin, CommandRejection> {
        match &service.origin {
            ServiceOrigin::Dynamic(origin) => Ok(origin),
            ServiceOrigin::Configured => Err(CommandRejection::InvalidState(
                "the service is configured, not dynamic".to_string(),
            )),
        }
    }

    fn live_dynamic_count(&self, services: &ServiceMap) -> usize {
        services
            .iter()
            .filter(|(id, service)| {
                roster_entry_state(&self.services, id, service)
                    == Some(RosterEntryState::LiveDynamic)
            })
            .count()
    }

    fn require_dynamic_slot(&self, services: &ServiceMap) -> Result<(), CommandRejection> {
        let live = self.live_dynamic_count(services);
        if live < self.dynamic_policy.max_services {
            Ok(())
        } else {
            Err(CommandRejection::LimitExceeded(format!(
                "control.dynamic_services.max_services is {}; {live} dynamic services are live",
                self.dynamic_policy.max_services
            )))
        }
    }

    fn start_dynamic(
        &mut self,
        services: &mut ServiceMap,
        params: DynamicServiceParams,
    ) -> Result<DynamicServiceAck, CommandRejection> {
        self.require_dynamic_enabled()?;
        Self::validate_service_id(&params.service)?;
        let digest = Self::request_digest("start", None, &params)?;
        if let Some(ack) =
            self.replay_idempotent(params.idempotency_key.as_deref(), digest, services)?
        {
            return Ok(ack);
        }
        if services.contains_key(&params.service) {
            let retired_dynamic = services.get(&params.service).is_some_and(|service| {
                roster_entry_state(&self.services, &params.service, service)
                    == Some(RosterEntryState::RetiredDynamic)
            });
            let guidance = if retired_dynamic {
                "; use replace_dynamic_service to revive the retired entry"
            } else {
                ""
            };
            return Err(CommandRejection::InvalidSpec(format!(
                "service `{}` already exists{guidance}",
                params.service,
            )));
        }
        self.require_dynamic_slot(services)?;

        let (spec, lifetime) = self.materialize_spec(services, &params)?;
        let (expires_at, expires_at_unix_ms) = Self::lease_deadlines(lifetime)?;
        let now = unix_now_ms().unwrap_or_default();
        let origin = DynamicOrigin {
            created_at_unix_ms: now,
            expires_at_unix_ms,
            owner: params.owner.clone(),
            revision: 1,
        };
        let service = Service::dynamic(
            params.service.clone(),
            spec.clone(),
            ServiceOrigin::Dynamic(origin),
            self.default_log_retention,
        );
        self.validate_candidate(services, &service)?;

        let mut runtime = ServiceRuntime::new(ServiceRuntimeInit::from(&service));
        runtime.expires_at = expires_at;
        runtime.request_enable();
        let (snapshot, _) = project_snapshot(&service, &runtime);
        services.insert(params.service.clone(), service.clone());
        self.services.insert(params.service.clone(), runtime);
        self.writer.insert_service(snapshot, service.log_retention);
        self.append_event(
            &params.service,
            ServiceEventKind::Created,
            format!(
                "dynamic service created with revision 1; {}",
                Self::lease_detail(expires_at_unix_ms)
            ),
        );

        let ack = DynamicServiceAck::new(
            params.service,
            1,
            0,
            expires_at_unix_ms,
            &spec,
            false,
            false,
        );
        self.remember_idempotent(params.idempotency_key, digest, &ack);
        Ok(ack)
    }

    fn replace_dynamic(
        &mut self,
        services: &mut ServiceMap,
        service_id: &ServiceID,
        expected_revision: u64,
        params: DynamicServiceParams,
    ) -> Result<DynamicServiceAck, CommandRejection> {
        self.require_dynamic_enabled()?;
        if params.service != *service_id {
            return Err(CommandRejection::InvalidSpec(format!(
                "params.service `{}` does not match target `{service_id}`",
                params.service
            )));
        }
        let digest = Self::request_digest("replace", Some(expected_revision), &params)?;
        if let Some(ack) =
            self.replay_idempotent(params.idempotency_key.as_deref(), digest, services)?
        {
            return Ok(ack);
        }
        let current = services
            .get(service_id)
            .ok_or(CommandRejection::UnknownService)?;
        let current_origin = Self::dynamic_origin(current)?;
        if current_origin.revision != expected_revision {
            return Err(CommandRejection::RevisionMismatch {
                expected: expected_revision,
                actual: current_origin.revision,
            });
        }
        let reviving = roster_entry_state(&self.services, service_id, current)
            == Some(RosterEntryState::RetiredDynamic);
        if reviving {
            self.require_dynamic_slot(services)?;
        }
        let created_at_unix_ms = current_origin.created_at_unix_ms;
        let owner = params
            .owner
            .clone()
            .or_else(|| current_origin.owner.clone());
        let revision = current_origin.revision.checked_add(1).ok_or_else(|| {
            CommandRejection::InvalidSpec(format!(
                "service `{service_id}` has exhausted its revision counter"
            ))
        })?;
        let (spec, lifetime) = self.materialize_spec(services, &params)?;
        let (expires_at, expires_at_unix_ms) = Self::lease_deadlines(lifetime)?;
        let origin = DynamicOrigin {
            created_at_unix_ms,
            expires_at_unix_ms,
            owner,
            revision,
        };
        let mut candidate = current.clone();
        candidate.spec = spec.clone();
        candidate.origin = ServiceOrigin::Dynamic(origin.clone());
        self.validate_candidate(services, &candidate)?;

        let runtime = self
            .services
            .get_mut(service_id)
            .ok_or(CommandRejection::UnknownService)?;
        let observed_generation = runtime.run_generation();
        runtime.retired = None;
        runtime.retired_at_unix_ms = None;
        runtime.expires_at = expires_at;
        runtime.reconfigure(&spec.restart);
        runtime.request_restart();
        let service = services
            .get_mut(service_id)
            .ok_or(CommandRejection::UnknownService)?;
        service.spec = spec.clone();
        service.origin = ServiceOrigin::Dynamic(origin);
        self.sync(services, service_id);
        self.append_event(
            service_id,
            ServiceEventKind::Replaced,
            format!(
                "dynamic service replaced with revision {revision}; {}",
                Self::lease_detail(expires_at_unix_ms)
            ),
        );

        let ack = DynamicServiceAck::new(
            service_id.clone(),
            revision,
            observed_generation,
            expires_at_unix_ms,
            &spec,
            false,
            false,
        );
        self.remember_idempotent(params.idempotency_key, digest, &ack);
        Ok(ack)
    }

    fn stop_dynamic(
        &mut self,
        services: &ServiceMap,
        service_id: &ServiceID,
    ) -> Result<DynamicServiceAck, CommandRejection> {
        self.require_dynamic_enabled()?;
        let service = services
            .get(service_id)
            .ok_or(CommandRejection::UnknownService)?;
        let origin = Self::dynamic_origin(service)?;
        let runtime = self
            .services
            .get_mut(service_id)
            .ok_or(CommandRejection::UnknownService)?;
        let observed_generation = runtime.run_generation();
        let already_retired = runtime.is_retired();
        if !already_retired {
            runtime.disable();
            runtime.retired = Some(RetiredReason::Stopped);
            runtime.retired_at_unix_ms = unix_now_ms();
            runtime.expires_at = None;
            self.sync(services, service_id);
            self.append_event(
                service_id,
                ServiceEventKind::Retired,
                "dynamic service retired because stop was requested",
            );
        }
        Ok(DynamicServiceAck::new(
            service_id.clone(),
            origin.revision,
            observed_generation,
            origin.expires_at_unix_ms,
            &service.spec,
            already_retired,
            false,
        ))
    }

    fn renew_dynamic(
        &mut self,
        services: &mut ServiceMap,
        service_id: &ServiceID,
        expected_revision: u64,
        expires_after: Option<Lease>,
    ) -> Result<DynamicServiceAck, CommandRejection> {
        self.require_dynamic_enabled()?;
        let service = services
            .get(service_id)
            .ok_or(CommandRejection::UnknownService)?;
        let origin = Self::dynamic_origin(service)?;
        if origin.revision != expected_revision {
            return Err(CommandRejection::RevisionMismatch {
                expected: expected_revision,
                actual: origin.revision,
            });
        }
        let runtime = self
            .services
            .get(service_id)
            .ok_or(CommandRejection::UnknownService)?;
        if runtime.is_retired() {
            return Err(CommandRejection::InvalidState(
                "the dynamic service is retired; use replace_dynamic_service to revive it"
                    .to_string(),
            ));
        }

        let lifetime = self.effective_lifetime(expires_after);
        let (expires_at, expires_at_unix_ms) = Self::lease_deadlines(lifetime)?;
        let observed_generation = runtime.run_generation();
        let revision = origin.revision;
        let spec = service.spec.clone();

        // Re-borrowing mutably what the checks above already validated; the scheduler is
        // single-threaded, so neither entry can have changed in between.
        if let Some(runtime) = self.services.get_mut(service_id) {
            runtime.expires_at = expires_at;
        }
        if let Some(ServiceOrigin::Dynamic(origin)) = services
            .get_mut(service_id)
            .map(|service| &mut service.origin)
        {
            origin.expires_at_unix_ms = expires_at_unix_ms;
        }
        self.sync(services, service_id);
        self.append_event(
            service_id,
            ServiceEventKind::LeaseRenewed,
            format!(
                "dynamic service lease renewed; {}",
                Self::lease_detail(expires_at_unix_ms)
            ),
        );

        Ok(DynamicServiceAck::new(
            service_id.clone(),
            revision,
            observed_generation,
            expires_at_unix_ms,
            &spec,
            false,
            false,
        ))
    }

    fn expire_due(&mut self, services: &ServiceMap) -> bool {
        let now = tokio::time::Instant::now();
        let due = self
            .services
            .iter()
            .filter(|(_, runtime)| {
                runtime.is_live() && runtime.expires_at.is_some_and(|deadline| deadline <= now)
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for service_id in &due {
            if let Some(runtime) = self.services.get_mut(service_id) {
                runtime.disable();
                runtime.retired = Some(RetiredReason::Expired);
                runtime.retired_at_unix_ms = unix_now_ms();
                runtime.expires_at = None;
            }
            self.sync(services, service_id);
            self.append_event(
                service_id,
                ServiceEventKind::Retired,
                "dynamic service retired because its lease expired",
            );
        }
        !due.is_empty()
    }

    fn next_expiry(&self) -> Option<tokio::time::Instant> {
        self.services
            .values()
            .filter(|runtime| runtime.is_live())
            .filter_map(|runtime| runtime.expires_at)
            .min()
    }

    fn evict_retired(&mut self, services: &mut ServiceMap) {
        loop {
            let retired_count = self
                .services
                .values()
                .filter(|runtime| runtime.is_retired())
                .count();
            if retired_count <= MAX_RETIRED_SERVICES {
                return;
            }
            let depended_on = services
                .iter()
                .filter(|(id, _)| self.services.get(*id).is_some_and(ServiceRuntime::is_live))
                .flat_map(|(_, service)| {
                    service
                        .spec
                        .depends_on
                        .iter()
                        .map(|dependency| dependency.service.clone())
                })
                .collect::<std::collections::HashSet<_>>();
            let candidate = services
                .keys()
                .filter_map(|id| {
                    let runtime = self.services.get(id)?;
                    (runtime.is_retired()
                        && runtime.running.is_none()
                        && runtime.draining_log_readers.is_empty()
                        && !depended_on.contains(id))
                    .then_some((runtime.retired_at_unix_ms.unwrap_or_default(), id.clone()))
                })
                .min();
            let Some((_, id)) = candidate else {
                return;
            };
            self.services.remove(&id);
            services.shift_remove(&id);
            self.writer.remove_service(&id);
        }
    }

    fn reload_services(&mut self, services: &mut ServiceMap) -> Result<(), CommandRejection> {
        let Some(reload) = &self.reload_config else {
            return Ok(());
        };
        let updated = load_services_from_disk(reload).map_err(CommandRejection::ConfigReload)?;
        validate_reloaded_services(services, &self.services, &updated)
            .map_err(CommandRejection::ConfigReload)?;

        let mut merged = updated;
        for (service_id, service) in services.iter().filter(|(service_id, service)| {
            matches!(
                roster_entry_state(&self.services, service_id, service),
                Some(
                    RosterEntryState::RetiredConfigured
                        | RosterEntryState::LiveDynamic
                        | RosterEntryState::RetiredDynamic
                )
            )
        }) {
            merged.insert(service_id.clone(), service.clone());
        }
        ServiceGraph::new(&merged)
            .map_err(|err| CommandRejection::ConfigReload(err.to_string()))?;

        let changed = services
            .iter()
            .filter(|(service_id, service)| {
                roster_entry_state(&self.services, service_id, service)
                    == Some(RosterEntryState::LiveConfigured)
            })
            .filter_map(|(service_id, service)| {
                merged
                    .get(service_id)
                    .filter(|updated| updated.spec != service.spec)
                    .map(|_| service_id.clone())
            })
            .collect::<Vec<_>>();

        for (service_id, runtime) in &mut self.services {
            if let Some(service) = merged
                .get(service_id)
                .filter(|service| matches!(service.origin, ServiceOrigin::Configured))
            {
                runtime.reconfigure(&service.spec.restart);
                self.writer
                    .reconfigure_log_retention(service_id, service.log_retention);
            }
        }
        *services = merged;
        self.sync_all(services);
        if !changed.is_empty() {
            let detail = format!("reloaded changed service specs: {}", changed.join(", "));
            for service_id in &changed {
                self.append_event(service_id, ServiceEventKind::ConfigReloaded, detail.clone());
            }
        }
        Ok(())
    }

    fn reconcile_actions(
        &self,
        services: &ServiceMap,
        updated: &ServiceMap,
    ) -> Vec<ReconcileAction> {
        let mut actions = Vec::new();
        for (service_id, updated_service) in updated {
            let current = services.get(service_id).filter(|service| {
                roster_entry_state(&self.services, service_id, service)
                    == Some(RosterEntryState::LiveConfigured)
            });
            let Some(current) = current else {
                actions.push(ReconcileAction {
                    service: service_id.clone(),
                    action: ReconcileActionKind::Added,
                    detail: "configured service added".to_string(),
                });
                continue;
            };

            let mut changed = Vec::new();
            if current.spec != updated_service.spec {
                changed.push("service spec");
            }
            if current.startup_mode != updated_service.startup_mode {
                changed.push("startup mode");
            }
            if current.log_retention != updated_service.log_retention {
                changed.push("log retention");
            }
            if current.enable_color != updated_service.enable_color {
                changed.push("color");
            }
            if !changed.is_empty() {
                actions.push(ReconcileAction {
                    service: service_id.clone(),
                    action: ReconcileActionKind::Changed,
                    detail: format!("changed {}", changed.join(", ")),
                });
            }
        }
        actions.extend(
            services
                .iter()
                .filter(|(service_id, service)| {
                    roster_entry_state(&self.services, service_id, service)
                        == Some(RosterEntryState::LiveConfigured)
                        && !updated.contains_key(*service_id)
                })
                .map(|(service_id, _)| ReconcileAction {
                    service: service_id.clone(),
                    action: ReconcileActionKind::Removed,
                    detail: "configured service removed".to_string(),
                }),
        );
        actions.sort_by(|left, right| left.service.cmp(&right.service));
        actions
    }

    fn validate_reconcile_candidate(
        &self,
        services: &ServiceMap,
        updated: &ServiceMap,
        actions: &[ReconcileAction],
    ) -> Result<(), CommandRejection> {
        if let Some(service_id) = updated.keys().find(|service_id| {
            services
                .get(*service_id)
                .is_some_and(|service| matches!(service.origin, ServiceOrigin::Dynamic(_)))
        }) {
            return Err(CommandRejection::InvalidSpec(format!(
                "service `{service_id}` exists as a live or retired dynamic service; stop it or rename the configured service"
            )));
        }

        let removed = actions
            .iter()
            .filter(|action| action.action == ReconcileActionKind::Removed)
            .map(|action| action.service.as_str())
            .collect::<std::collections::HashSet<_>>();
        let mut dependents = services
            .iter()
            .filter(|(service_id, service)| {
                roster_entry_state(&self.services, service_id, service)
                    == Some(RosterEntryState::LiveDynamic)
                    && service
                        .spec
                        .depends_on
                        .iter()
                        .any(|dependency| removed.contains(dependency.service.as_str()))
            })
            .map(|(service_id, _)| service_id.clone())
            .collect::<Vec<_>>();
        dependents.sort();
        if !dependents.is_empty() {
            return Err(CommandRejection::InvalidSpec(format!(
                "cannot remove configured services while live dynamic services depend on them: {}",
                dependents.join(", ")
            )));
        }

        let mut candidate = updated.clone();
        for (service_id, service) in services {
            let state = roster_entry_state(&self.services, service_id, service);
            let preserved_dynamic = matches!(
                state,
                Some(RosterEntryState::LiveDynamic | RosterEntryState::RetiredDynamic)
            );
            let already_retired_configured = state == Some(RosterEntryState::RetiredConfigured)
                && !updated.contains_key(service_id);
            let retained_tombstone =
                removed.contains(service_id.as_str()) || already_retired_configured;
            if preserved_dynamic || retained_tombstone {
                candidate.insert(service_id.clone(), service.clone());
            }
        }
        ServiceGraph::new(&candidate)
            .map(|_| ())
            .map_err(|err| CommandRejection::InvalidSpec(err.to_string()))
    }

    fn apply_reconcile(
        &mut self,
        services: &mut ServiceMap,
        updated: &ServiceMap,
        actions: &[ReconcileAction],
    ) {
        let by_service = actions
            .iter()
            .map(|action| (action.service.as_str(), action))
            .collect::<HashMap<_, _>>();
        for (service_id, service) in updated {
            let Some(action) = by_service.get(service_id.as_str()) else {
                continue;
            };
            match action.action {
                ReconcileActionKind::Added => {
                    let reviving = services.get(service_id).is_some_and(|current| {
                        roster_entry_state(&self.services, service_id, current)
                            == Some(RosterEntryState::RetiredConfigured)
                    });
                    if reviving {
                        if let Some(runtime) = self.services.get_mut(service_id) {
                            runtime.retired = None;
                            runtime.retired_at_unix_ms = None;
                            runtime.expires_at = None;
                            runtime.reconfigure(&service.spec.restart);
                            match service.startup_mode {
                                StartupMode::Enabled => runtime.request_restart(),
                                StartupMode::Disabled => runtime.disable(),
                            }
                        }
                        services.insert(service_id.clone(), service.clone());
                        self.writer
                            .reconfigure_log_retention(service_id, service.log_retention);
                        self.sync(services, service_id);
                    } else {
                        let runtime = ServiceRuntime::new(ServiceRuntimeInit::from(service));
                        let (snapshot, _) = project_snapshot(service, &runtime);
                        services.insert(service_id.clone(), service.clone());
                        self.services.insert(service_id.clone(), runtime);
                        self.writer.insert_service(snapshot, service.log_retention);
                    }
                    self.append_event(
                        service_id,
                        ServiceEventKind::Created,
                        if reviving {
                            "configured service revived by config reconciliation"
                        } else {
                            "configured service added by config reconciliation"
                        },
                    );
                }
                ReconcileActionKind::Changed => {
                    if let Some(runtime) = self.services.get_mut(service_id) {
                        runtime.reconfigure(&service.spec.restart);
                    }
                    services.insert(service_id.clone(), service.clone());
                    self.writer
                        .reconfigure_log_retention(service_id, service.log_retention);
                    self.sync(services, service_id);
                    self.append_event(
                        service_id,
                        ServiceEventKind::ConfigReloaded,
                        format!("config reconciliation {}", action.detail),
                    );
                }
                ReconcileActionKind::Removed => {}
            }
        }
        for action in actions
            .iter()
            .filter(|action| action.action == ReconcileActionKind::Removed)
        {
            let service_id = &action.service;
            if let Some(runtime) = self.services.get_mut(service_id) {
                runtime.disable();
                runtime.retired = Some(RetiredReason::Removed);
                runtime.retired_at_unix_ms = unix_now_ms();
                runtime.expires_at = None;
            }
            self.sync(services, service_id);
            self.append_event(
                service_id,
                ServiceEventKind::Retired,
                "configured service retired because it was removed by config reconciliation",
            );
        }
    }

    fn reconcile_config(
        &mut self,
        services: &mut ServiceMap,
        dry_run: bool,
    ) -> Result<ReconcileReceipt, CommandRejection> {
        let reload = self.reload_config.as_ref().ok_or_else(|| {
            CommandRejection::ConfigReload(
                "session has no config path; config reconciliation is unavailable".to_string(),
            )
        })?;
        let config_path = reload.config_path.display().to_string();
        let updated = load_services_from_disk(reload).map_err(CommandRejection::ConfigReload)?;
        let actions = self.reconcile_actions(services, &updated);
        // Validate on dry runs too: a preview that reports actions the apply would then
        // reject (dynamic-id collision, orphaned dependents, graph failure) is not a preview.
        self.validate_reconcile_candidate(services, &updated, &actions)?;
        if !dry_run {
            self.apply_reconcile(services, &updated, &actions);
        }
        Ok(ReconcileReceipt {
            config_path,
            dry_run,
            actions,
        })
    }

    fn has_due_auto_restart(&self, services: &ServiceMap) -> bool {
        let now = tokio::time::Instant::now();
        services.iter().any(|(service_id, service)| {
            let Some(runtime) = self.services.get(service_id) else {
                return false;
            };
            if runtime.desired == DesiredState::Disabled
                || runtime.running.is_some()
                || runtime.start_requested
            {
                return false;
            }
            if runtime
                .restart
                .backoff_until
                .is_some_and(|deadline| now < deadline)
            {
                return false;
            }
            match runtime.state {
                State::Exited { exit_code } => {
                    runtime.will_auto_restart(&service.spec.restart, exit_code)
                }
                State::Pending
                | State::Starting
                | State::Running { .. }
                | State::Killed
                | State::Disabled => false,
            }
        })
    }

    fn reload_before_auto_restart(&mut self, services: &mut ServiceMap) {
        if self.reload_config.is_none() || !self.has_due_auto_restart(services) {
            return;
        }
        if let Err(err) = self.reload_services(services) {
            tracing::warn!(
                ?err,
                "config reload before automatic restart failed; keeping previous service definitions"
            );
        }
    }

    fn schedule_pass(&mut self, services: &mut ServiceMap) {
        self.reload_before_auto_restart(services);
        schedule::schedule_ready(&mut schedule::ScheduleContext {
            services,
            runtimes: &mut self.services,
            current_pty_size: self.current_pty_size,
            events_tx: &self.events_tx,
            #[cfg(test)]
            test_events: &mut self.test_events,
            writer: &self.writer,
            shutdown: &self.shutdown,
        });
    }

    fn reply(ack: Option<CommandAck>, result: ServiceCommandResult) {
        match ack {
            Some(ack) => ack.send(result),
            // Fire-and-forget senders (the TUI) have nowhere to surface a rejection; leave a
            // trace so an inert keypress is diagnosable.
            None => {
                if let Err(rejection) = result {
                    tracing::debug!(?rejection, "fire-and-forget command rejected");
                }
            }
        }
    }

    fn reply_dynamic(ack: Option<control::DynamicCommandAck>, result: DynamicServiceResult) {
        match ack {
            Some(ack) => ack.send(result),
            None => {
                if let Err(rejection) = result {
                    tracing::debug!(?rejection, "fire-and-forget dynamic command rejected");
                }
            }
        }
    }

    /// Restart a service, latching the run generation *before* the restart. Restarting a disabled
    /// service is invalid for every caller: `enable` is the operation that starts disabled services.
    fn apply_restart(
        &mut self,
        services: &mut ServiceMap,
        service_id: &ServiceID,
    ) -> ServiceCommandResult {
        if !self.services.contains_key(service_id) {
            return Err(CommandRejection::UnknownService);
        }
        let runtime = self
            .services
            .get(service_id)
            .ok_or(CommandRejection::UnknownService)?;
        if let Some(reason) = runtime.retired {
            return Err(CommandRejection::InvalidState(
                Self::retired_guidance(reason).to_string(),
            ));
        }
        if runtime.desired == DesiredState::Disabled {
            return Err(CommandRejection::InvalidState(
                "the service is disabled; enable it before restarting".to_string(),
            ));
        }
        self.reload_services(services)?;
        let runtime = self
            .services
            .get_mut(service_id)
            .ok_or(CommandRejection::UnknownService)?;
        let observed_generation = runtime.run_generation();
        // Logs are cleared when the service is actually (re)started in `start_service_if_ready`,
        // after the old process has drained its output.
        runtime.request_restart();
        self.sync(services, service_id);
        self.append_event(
            service_id,
            ServiceEventKind::RestartRequested,
            "service restart requested",
        );
        Ok(vec![ServiceCommandAck {
            service: service_id.clone(),
            observed_generation,
        }])
    }

    fn apply_enable(
        &mut self,
        services: &mut ServiceMap,
        service_id: &ServiceID,
    ) -> ServiceCommandResult {
        if !self.services.contains_key(service_id) {
            return Err(CommandRejection::UnknownService);
        }
        if let Some(reason) = self
            .services
            .get(service_id)
            .and_then(|runtime| runtime.retired)
        {
            return Err(CommandRejection::InvalidState(
                Self::retired_guidance(reason).to_string(),
            ));
        }
        self.reload_services(services)?;
        let runtime = self
            .services
            .get_mut(service_id)
            .ok_or(CommandRejection::UnknownService)?;
        let observed_generation = runtime.run_generation();
        runtime.request_enable();
        self.sync(services, service_id);
        self.append_event(
            service_id,
            ServiceEventKind::EnableRequested,
            "service enable requested",
        );
        Ok(vec![ServiceCommandAck {
            service: service_id.clone(),
            observed_generation,
        }])
    }

    fn apply_disable(
        &mut self,
        services: &ServiceMap,
        service_id: &ServiceID,
    ) -> ServiceCommandResult {
        let Some(runtime) = self.services.get_mut(service_id) else {
            return Err(CommandRejection::UnknownService);
        };
        let observed_generation = runtime.run_generation();
        runtime.disable();
        self.sync(services, service_id);
        self.append_event(
            service_id,
            ServiceEventKind::DisableRequested,
            "service disable requested",
        );
        Ok(vec![ServiceCommandAck {
            service: service_id.clone(),
            observed_generation,
        }])
    }

    fn apply_restart_all(&mut self, services: &mut ServiceMap) -> ServiceCommandResult {
        self.reload_services(services)?;
        let mut acks = Vec::new();
        for service_id in services.keys() {
            let restart = self
                .services
                .get_mut(service_id)
                .filter(|runtime| runtime.desired == DesiredState::Enabled && runtime.is_live())
                .map(|runtime| {
                    let observed_generation = runtime.run_generation();
                    runtime.request_restart();
                    observed_generation
                });
            if let Some(observed_generation) = restart {
                self.sync(services, service_id);
                self.append_event(
                    service_id,
                    ServiceEventKind::RestartRequested,
                    "service restart requested by restart_all",
                );
                acks.push(ServiceCommandAck {
                    service: service_id.clone(),
                    observed_generation,
                });
            }
        }
        Ok(acks)
    }

    fn handle_command(&mut self, services: &mut ServiceMap, command: Command) -> bool {
        match command {
            Command::Restart { service, ack } => {
                let result = self.apply_restart(services, &service);
                Self::reply(ack, result);
                true
            }
            Command::Enable { service, ack } => {
                let result = self.apply_enable(services, &service);
                Self::reply(ack, result);
                true
            }
            Command::RestartAll { ack } => {
                let result = self.apply_restart_all(services);
                Self::reply(ack, result);
                true
            }
            Command::Disable { service, ack } => {
                let result = self.apply_disable(services, &service);
                #[cfg(test)]
                if result.is_ok() {
                    self.test_events.forward(Event::Disabled(service.clone()));
                }
                Self::reply(ack, result);
                true
            }
            Command::ReconcileConfig { dry_run, ack } => {
                let result = self.reconcile_config(services, dry_run);
                ack.send(result);
                true
            }
            Command::StartDynamic { params, ack } => {
                let result = self.start_dynamic(services, params);
                Self::reply_dynamic(ack, result);
                true
            }
            Command::ReplaceDynamic {
                service,
                expected_revision,
                params,
                ack,
            } => {
                let result = self.replace_dynamic(services, &service, expected_revision, params);
                Self::reply_dynamic(ack, result);
                true
            }
            Command::RenewDynamic {
                service,
                expected_revision,
                expires_after,
                ack,
            } => {
                let result =
                    self.renew_dynamic(services, &service, expected_revision, expires_after);
                Self::reply_dynamic(ack, result);
                true
            }
            Command::StopDynamic { service, ack } => {
                let result = self.stop_dynamic(services, &service);
                Self::reply_dynamic(ack, result);
                true
            }
            Command::SendInput(service_id, data) => {
                if let Some(runtime) = self.services.get(&service_id)
                    && let Some(running) = &runtime.running
                {
                    running.pty.write_input(&service_id, &data);
                }
                false
            }
            Command::ResizeAll { cols, rows } => {
                self.current_pty_size = portable_pty::PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                };
                for (service_id, runtime) in &self.services {
                    if let Some(running) = &runtime.running {
                        running.pty.resize(service_id, self.current_pty_size);
                    }
                }
                false
            }
        }
    }

    fn handle_health_event(
        &mut self,
        services: &ServiceMap,
        event: &ProcessEvent,
        health: Health,
        kind: ServiceEventKind,
        detail: &'static str,
    ) -> bool {
        let service_id = event.service_id();
        let changed = self
            .services
            .get_mut(service_id)
            .is_some_and(|runtime| runtime.mark_health(health));
        self.sync(services, service_id);
        if changed {
            self.append_event(service_id, kind, detail);
        }
        #[cfg(test)]
        self.test_events.forward(event.to_test_event());
        true
    }

    fn handle_exit_event(
        &mut self,
        services: &ServiceMap,
        event: &ProcessEvent,
        exit_code: i32,
    ) -> bool {
        let service_id = event.service_id();
        if let Some(service) = services.get(service_id)
            && let Some(runtime) = self.services.get_mut(service_id)
        {
            runtime.finish_current_run(&service.spec.restart, exit_code);
        }
        self.sync(services, service_id);
        let generation = event.run_id().get();
        let mut exited = service_event(
            generation,
            ServiceEventKind::Exited,
            format!("service process exited with code {exit_code}"),
        );
        exited.exit_code = Some(exit_code);
        self.writer.append_event(service_id, exited);
        if let Some(delay) = self.services.get(service_id).and_then(|runtime| {
            runtime
                .restart
                .backoff_until
                .and(runtime.restart.backoff_delay)
        }) {
            let mut backoff = service_event(
                generation,
                ServiceEventKind::BackoffScheduled,
                format!("automatic restart scheduled after {} ms", delay.as_millis()),
            );
            backoff.delay_ms = u64::try_from(delay.as_millis()).ok();
            self.writer.append_event(service_id, backoff);
        }
        #[cfg(test)]
        self.test_events.forward(event.to_test_event());
        true
    }

    fn handle_event(&mut self, services: &ServiceMap, event: &ProcessEvent) -> bool {
        tracing::debug!(?event, "received process event");

        let service_id = event.service_id().clone();
        let Some(runtime) = self.services.get(&service_id) else {
            return false;
        };
        let current_run_id = runtime.current_run_id();
        let log_reader_finished = matches!(event, ProcessEvent::LogReaderFinished { .. })
            && runtime
                .draining_log_readers
                .iter()
                .any(|(run_id, _)| *run_id == event.run_id());
        if current_run_id != Some(event.run_id()) && !log_reader_finished {
            tracing::debug!(
                service_id,
                event_run_id = ?event.run_id(),
                current_run_id = ?current_run_id,
                "ignoring stale process event"
            );
            return false;
        }

        match event {
            ProcessEvent::Healthy { .. } => self.handle_health_event(
                services,
                event,
                Health::Healthy,
                ServiceEventKind::Healthy,
                "service became healthy",
            ),
            ProcessEvent::Unhealthy { .. } => self.handle_health_event(
                services,
                event,
                Health::Unhealthy,
                ServiceEventKind::Unhealthy,
                "service became unhealthy",
            ),
            ProcessEvent::Killed { .. } => {
                if let Some(runtime) = self.services.get_mut(&service_id) {
                    runtime.mark_killed();
                }
                self.sync(services, &service_id);
                #[cfg(test)]
                self.test_events.forward(event.to_test_event());
                true
            }
            ProcessEvent::Exited { exit_code, .. } => {
                self.handle_exit_event(services, event, *exit_code)
            }
            ProcessEvent::LogReaderFinished { run_id, .. } => {
                if let Some(runtime) = self.services.get_mut(&service_id) {
                    runtime.finish_log_reader(*run_id);
                }
                #[cfg(test)]
                self.test_events.forward(event.to_test_event());
                false
            }
        }
    }

    fn next_backoff(&self) -> Option<tokio::time::Instant> {
        let now = tokio::time::Instant::now();
        self.services
            .values()
            .filter_map(|runtime| runtime.restart.backoff_until)
            .filter(|deadline| *deadline > now)
            .min()
    }

    fn running_count(&self) -> usize {
        self.services
            .values()
            .filter(|runtime| runtime.running.is_some())
            .count()
    }

    fn cancel_all_running(&mut self) {
        for runtime in self.services.values_mut() {
            if let Some(running) = &mut runtime.running {
                running.cancel();
            }
        }
    }

    /// Keep the runtime alive after a shutdown so the per-service termination tasks can finish
    /// their SIGTERM -> deadline -> SIGKILL escalation and reap their children.
    ///
    /// Without this drain the tokio runtime would be dropped the instant the scheduler returns,
    /// aborting those detached tasks mid-escalation and orphaning any process that ignores
    /// SIGTERM. Each matching `Exited` event removes the service's run handle, so the drain ends as
    /// soon as every child has been reaped (bounded by an overall timeout).
    async fn drain_on_shutdown(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
    ) {
        if self.running_count() == 0 {
            return;
        }
        tracing::debug!(
            remaining = self.running_count(),
            "draining services on shutdown"
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while self.running_count() > 0 {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {
                    tracing::warn!(
                        remaining = self.running_count(),
                        "timed out waiting for services to exit"
                    );
                    break;
                }
                event = events_rx.recv() => {
                    let Some(event) = event else { break };
                    let _ = self.handle_event(services, &event);
                }
            }
        }
    }
}

pub(crate) struct SchedulerInput {
    pub(crate) services: ServiceMap,
    pub(crate) reload_config: Option<ReloadConfig>,
    pub(crate) commands_rx: mpsc::Receiver<Command>,
    pub(crate) events_rx: mpsc::Receiver<ProcessEvent>,
    pub(crate) events_tx: mpsc::Sender<ProcessEvent>,
    #[cfg(test)]
    pub(crate) test_events_tx: Option<mpsc::Sender<Event>>,
    pub(crate) writer: SessionModelWriter,
    pub(crate) shutdown: CancellationToken,
    pub(crate) config_dir: PathBuf,
    pub(crate) dynamic_policy: DynamicServicesPolicy,
    pub(crate) default_log_retention: LogRetention,
}

pub(crate) async fn scheduler(input: SchedulerInput) -> eyre::Result<()> {
    let SchedulerInput {
        mut services,
        reload_config,
        mut commands_rx,
        mut events_rx,
        events_tx,
        #[cfg(test)]
        test_events_tx,
        writer,
        shutdown,
        config_dir,
        dynamic_policy,
        default_log_retention,
    } = input;
    ServiceGraph::new(&services)?;
    #[cfg(test)]
    let test_events = {
        let tx = test_events_tx.unwrap_or_else(|| {
            let (tx, _rx) = mpsc::channel(1);
            tx
        });
        TestEventSink::new(tx)
    };
    let mut rt = SchedulerRuntime::new(
        &services,
        SchedulerResources {
            reload_config,
            events_tx,
            #[cfg(test)]
            test_events,
            writer,
            shutdown: shutdown.clone(),
            config_dir,
            dynamic_policy,
            default_log_retention,
        },
    );

    // Initial scheduling pass
    tracing::debug!("started initial scheduling pass");
    rt.schedule_pass(&mut services);
    tracing::debug!("completed initial scheduling pass");

    // Whenever an event comes in, try to (re)start any services whose deps are now healthy
    loop {
        tracing::debug!("waiting for scheduling event");
        // Wake the loop when the nearest pending restart backoff expires; without this a
        // backed-off service would never restart unless some unrelated event happened to arrive.
        let next_backoff = rt.next_backoff();
        let next_expiry = rt.next_expiry();
        let needs_schedule = tokio::select! {
            () = shutdown.cancelled() => {
                tracing::debug!("exiting scheduler");
                break;
            }
            command = commands_rx.recv() => {
                let Some(command) = command else {
                    break;
                };
                rt.handle_command(&mut services, command)
            }
            event = events_rx.recv() => {
                let Some(event) = event else {
                    break;
                };
                rt.handle_event(&services, &event)
            }
            () = async {
                match next_backoff {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => true,
            () = async {
                match next_expiry {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => rt.expire_due(&services),
        };

        // Retirement can become evictable only after later process/log-reader events arrive, so
        // enforce the bounded tombstone roster after every scheduler wake rather than only on
        // insertion.
        rt.evict_retired(&mut services);
        if needs_schedule {
            rt.schedule_pass(&mut services);
        }
    }

    rt.cancel_all_running();
    rt.drain_on_shutdown(&services, &mut events_rx).await;
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "scheduler/tests.rs"]
mod tests;
