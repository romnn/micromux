use crate::{
    ReloadConfig, ServiceMap,
    graph::ServiceGraph,
    health_check::Health,
    model::{Desired, Execution, ServiceSnapshot, SessionModelWriter},
    service::{self, Service},
};
use codespan_reporting::diagnostic::Severity;
use color_eyre::eyre;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Initial delay before automatically restarting a service after it exits.
const RESTART_BACKOFF_BASE: Duration = Duration::from_millis(250);
/// Maximum delay the (exponentially doubling) restart backoff grows to.
const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(10);
/// Minimum uptime after which a service is considered stable and its backoff is reset.
const RESTART_BACKOFF_RESET: Duration = RESTART_BACKOFF_MAX;

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
    CommandRejection, SchedulerStopped, ServiceCommandAck, ServiceCommandResult, ServiceControl,
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
}

pub(super) struct RunningService {
    run_id: RunId,
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
#[derive(Clone)]
pub(super) struct RunConfig {
    command: Vec<String>,
    working_dir: Option<String>,
    advertised_ports: Vec<u16>,
    healthcheck_configured: bool,
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
    /// When the current run started (wall clock), used to compute live uptime. `Some` iff running.
    last_started_at: Option<std::time::Instant>,
    /// Exit code of the most recently finished run.
    last_exit_code: Option<i32>,
    /// Fields that describe the most recent run rather than the current config.
    run_config: Option<RunConfig>,
    draining_log_readers: Vec<(RunId, pty::LogReaderHandle)>,
}

impl ServiceRuntime {
    fn new(policy: &service::RestartPolicy) -> Self {
        Self {
            desired: DesiredState::Enabled,
            start_requested: false,
            clear_logs_on_start: false,
            next_run_id: 0,
            running: None,
            restart: RestartTracker::new(policy),
            state: State::Pending,
            last_run_id: None,
            last_started_at: None,
            last_exit_code: None,
            run_config: None,
            draining_log_readers: Vec::new(),
        }
    }

    fn reconfigure(&mut self, policy: &service::RestartPolicy) {
        self.restart.reconfigure(policy);
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
        self.last_started_at = Some(std::time::Instant::now());
        self.running = Some(running);
        self.state = State::Running { health: None };
    }

    /// Mark a spawn attempt in progress.
    fn mark_starting(&mut self) {
        self.state = State::Starting;
    }

    /// Update the cached health from a resolved probe, but only while a process is live.
    fn mark_health(&mut self, health: Health) {
        if matches!(self.state, State::Running { .. } | State::Starting) {
            self.state = State::Running {
                health: Some(health),
            };
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
        self.last_started_at = None;
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
    let run_config = runtime.run_config.as_ref();
    let snapshot = ServiceSnapshot {
        id: service.id.clone(),
        name: service.name.as_ref().clone(),
        desired: match runtime.desired {
            DesiredState::Enabled => Desired::Enabled,
            DesiredState::Disabled => Desired::Disabled,
        },
        execution,
        health,
        run_generation: runtime.run_generation(),
        advertised_ports: run_config.map_or_else(
            || service.advertised_ports.clone(),
            |config| config.advertised_ports.clone(),
        ),
        healthcheck_configured: run_config.map_or_else(
            || service.health_check.is_some(),
            |config| config.healthcheck_configured,
        ),
        last_exit_code: runtime.last_exit_code,
        command: run_config.map_or_else(|| service.argv(), |config| config.command.clone()),
        working_dir: run_config.map_or_else(
            || service.working_dir_display(),
            |config| config.working_dir.clone(),
        ),
        uptime: None,
        restart_policy: service.restart_policy.clone(),
    };
    (snapshot, runtime.last_started_at)
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

fn validate_reloaded_services(current: &ServiceMap, updated: &ServiceMap) -> Result<(), String> {
    let missing = current
        .keys()
        .filter(|service_id| !updated.contains_key(*service_id))
        .cloned()
        .collect::<Vec<_>>();
    let added = updated
        .keys()
        .filter(|service_id| !current.contains_key(*service_id))
        .cloned()
        .collect::<Vec<_>>();

    match (missing.is_empty(), added.is_empty()) {
        (true, true) => Ok(()),
        (false, true) => Err(format!(
            "config reload cannot remove services while the session is running: {}",
            missing.join(", ")
        )),
        (true, false) => Err(format!(
            "config reload cannot add services while the session is running: {}",
            added.join(", ")
        )),
        (false, false) => Err(format!(
            "config reload cannot add/remove services while the session is running; removed: {}; added: {}",
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
}

impl SchedulerRuntime {
    fn new(
        services: &ServiceMap,
        reload_config: Option<ReloadConfig>,
        events_tx: mpsc::Sender<ProcessEvent>,
        #[cfg(test)] test_events: TestEventSink,
        writer: SessionModelWriter,
        shutdown: CancellationToken,
    ) -> Self {
        let services = services
            .iter()
            .map(|(service_id, service)| {
                (
                    service_id.clone(),
                    ServiceRuntime::new(&service.restart_policy),
                )
            })
            .collect();

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

    fn reload_services(&mut self, services: &mut ServiceMap) -> Result<(), CommandRejection> {
        let Some(reload) = &self.reload_config else {
            return Ok(());
        };
        let updated = load_services_from_disk(reload).map_err(CommandRejection::ConfigReload)?;
        validate_reloaded_services(services, &updated).map_err(CommandRejection::ConfigReload)?;

        for (service_id, runtime) in &mut self.services {
            if let Some(service) = updated.get(service_id) {
                runtime.reconfigure(&service.restart_policy);
                self.writer
                    .reconfigure_log_retention(service_id, service.log_retention);
            }
        }
        *services = updated;
        self.sync_all(services);
        Ok(())
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
                    runtime.will_auto_restart(&service.restart_policy, exit_code)
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
        if let Some(ack) = ack {
            ack.send(result);
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
        if self
            .services
            .get(service_id)
            .is_some_and(|runtime| runtime.desired == DesiredState::Disabled)
        {
            return Err(CommandRejection::InvalidState);
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
        self.reload_services(services)?;
        let runtime = self
            .services
            .get_mut(service_id)
            .ok_or(CommandRejection::UnknownService)?;
        let observed_generation = runtime.run_generation();
        runtime.request_enable();
        self.sync(services, service_id);
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
                .filter(|runtime| runtime.desired == DesiredState::Enabled)
                .map(|runtime| {
                    let observed_generation = runtime.run_generation();
                    runtime.request_restart();
                    observed_generation
                });
            if let Some(observed_generation) = restart {
                self.sync(services, service_id);
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

    fn handle_event(&mut self, services: &ServiceMap, event: &ProcessEvent) -> bool {
        tracing::debug!(?event, "received process event");

        let service_id = event.service_id().clone();
        {
            let Some(runtime) = self.services.get(&service_id) else {
                return false;
            };
            let current_run_id = runtime.current_run_id();
            let model_only_for_last_run = current_run_id.is_none()
                && runtime.last_run_id == Some(event.run_id())
                && matches!(
                    event,
                    ProcessEvent::LogLine { .. }
                        | ProcessEvent::HealthCheckStarted { .. }
                        | ProcessEvent::HealthCheckLogLine { .. }
                        | ProcessEvent::HealthCheckFinished { .. }
                );
            let log_reader_finished = matches!(event, ProcessEvent::LogReaderFinished { .. })
                && runtime
                    .draining_log_readers
                    .iter()
                    .any(|(run_id, _)| *run_id == event.run_id());
            if current_run_id != Some(event.run_id())
                && !model_only_for_last_run
                && !log_reader_finished
            {
                tracing::debug!(
                    service_id,
                    event_run_id = ?event.run_id(),
                    current_run_id = ?current_run_id,
                    "ignoring stale process event"
                );
                return false;
            }
        }

        #[cfg(test)]
        let test_event = event.to_test_event();

        self.write_event_to_model(&service_id, event);

        match &event {
            ProcessEvent::LogLine { .. }
            | ProcessEvent::HealthCheckStarted { .. }
            | ProcessEvent::HealthCheckLogLine { .. }
            | ProcessEvent::HealthCheckFinished { .. } => {
                #[cfg(test)]
                self.test_events.forward(test_event);
                false
            }
            ProcessEvent::Healthy { .. } => {
                if let Some(runtime) = self.services.get_mut(&service_id) {
                    runtime.mark_health(Health::Healthy);
                }
                self.sync(services, &service_id);
                #[cfg(test)]
                self.test_events.forward(test_event);
                true
            }
            ProcessEvent::Unhealthy { .. } => {
                if let Some(runtime) = self.services.get_mut(&service_id) {
                    runtime.mark_health(Health::Unhealthy);
                }
                self.sync(services, &service_id);
                #[cfg(test)]
                self.test_events.forward(test_event);
                true
            }
            ProcessEvent::Killed { .. } => {
                if let Some(runtime) = self.services.get_mut(&service_id) {
                    runtime.mark_killed();
                }
                self.sync(services, &service_id);
                #[cfg(test)]
                self.test_events.forward(test_event);
                true
            }
            ProcessEvent::Exited { exit_code, .. } => {
                if let Some(service) = services.get(&service_id)
                    && let Some(runtime) = self.services.get_mut(&service_id)
                {
                    runtime.finish_current_run(&service.restart_policy, *exit_code);
                }
                self.sync(services, &service_id);
                #[cfg(test)]
                self.test_events.forward(test_event);
                true
            }
            ProcessEvent::LogReaderFinished { run_id, .. } => {
                if let Some(runtime) = self.services.get_mut(&service_id) {
                    runtime.finish_log_reader(*run_id);
                }
                #[cfg(test)]
                self.test_events.forward(test_event);
                false
            }
        }
    }

    fn write_event_to_model(&self, service_id: &ServiceID, event: &ProcessEvent) {
        // Write the model from the scheduler's own task — lossless from the scheduler onward.
        match event {
            ProcessEvent::LogLine {
                run_id,
                stream,
                update,
                line,
                ..
            } => {
                self.writer
                    .append_log(service_id, run_id.get(), *stream, *update, line.clone());
            }
            ProcessEvent::HealthCheckStarted {
                run_id,
                attempt,
                command,
                ..
            } => {
                self.writer.start_health_attempt(
                    service_id,
                    run_id.get(),
                    *attempt,
                    command.clone(),
                );
            }
            ProcessEvent::HealthCheckLogLine {
                run_id,
                attempt,
                stream,
                line,
                ..
            } => {
                self.writer.append_health_line(
                    service_id,
                    run_id.get(),
                    *attempt,
                    *stream,
                    line.clone(),
                );
            }
            ProcessEvent::HealthCheckFinished {
                run_id,
                attempt,
                success,
                exit_code,
                cancelled,
                ..
            } => {
                self.writer.finish_health_attempt(
                    service_id,
                    run_id.get(),
                    *attempt,
                    *success,
                    *exit_code,
                    *cancelled,
                );
            }
            ProcessEvent::Healthy { .. }
            | ProcessEvent::Unhealthy { .. }
            | ProcessEvent::Killed { .. }
            | ProcessEvent::Exited { .. }
            | ProcessEvent::LogReaderFinished { .. } => {}
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
        reload_config,
        events_tx,
        #[cfg(test)]
        test_events,
        writer,
        shutdown.clone(),
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
        };

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
