//! The narrow command capability handed to untrusted adapters (control server, MCP).
//!
//! [`ServiceControl`] exposes only the safe service operations and is **request/response**: each
//! method carries a oneshot reply, so the *scheduler* validates the command and latches the
//! generation at the exact moment it processes it — there is no pre-queue snapshot race. The type
//! has no `send_input`/`resize_all` method and cannot construct those `Command` variants, so raw
//! input forwarding is *unreachable* for adapters; the trusted in-process TUI keeps the full
//! `mpsc::Sender<Command>` instead.

use super::types::{Command, ServiceID};
use crate::{DynamicServiceParams, Lease, RestartPolicy, ServiceSpec};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

/// Acknowledgement for one affected service: the generation the scheduler latched while processing
/// the command. Reused directly as the control-protocol wire payload.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ServiceCommandAck {
    /// The affected service.
    pub service: ServiceID,
    /// The run generation latched at command-processing time (the generation *before* a
    /// restart/enable action; informational for disable).
    pub observed_generation: u64,
}

/// Scheduler acknowledgement for a dynamic-service mutation. Reused directly as the
/// control-protocol wire payload, like [`ServiceCommandAck`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DynamicServiceAck {
    /// Affected service.
    pub service: ServiceID,
    /// Current optimistic-concurrency revision.
    pub revision: u64,
    /// Run generation observed before the mutation.
    pub observed_generation: u64,
    /// Effective lease expiry in Unix milliseconds, or `None` for a session-lifetime lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_unix_ms: Option<u64>,
    /// Effective command argv.
    pub command: Vec<String>,
    /// Effective working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    /// Informational ports.
    #[serde(default)]
    pub ports: Vec<u16>,
    /// Environment key names. Values never leave the scheduler boundary.
    #[serde(default)]
    pub env_keys: Vec<String>,
    /// Effective restart policy.
    pub restart: RestartPolicy,
    /// Whether an effective healthcheck is configured.
    pub healthcheck_configured: bool,
    /// Whether a stop found the service already retired.
    #[serde(default)]
    pub already_retired: bool,
    /// Whether the acknowledgement was replayed from the idempotency window.
    #[serde(default)]
    pub idempotent_replay: bool,
}

impl DynamicServiceAck {
    pub(crate) fn new(
        service: ServiceID,
        revision: u64,
        observed_generation: u64,
        expires_at_unix_ms: Option<u64>,
        spec: &ServiceSpec,
        already_retired: bool,
        idempotent_replay: bool,
    ) -> Self {
        let mut env_keys = spec.environment.keys().cloned().collect::<Vec<_>>();
        env_keys.sort_unstable();
        Self {
            service,
            revision,
            observed_generation,
            expires_at_unix_ms,
            command: spec.command.clone(),
            working_dir: spec.working_dir_display(),
            ports: spec.ports.clone(),
            env_keys,
            restart: spec.restart.clone(),
            healthcheck_configured: spec.healthcheck.is_some(),
            already_retired,
            idempotent_replay,
        }
    }
}

/// A typed rejection produced by the scheduler when it declines a command.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandRejection {
    /// No service with the given id exists.
    #[error("unknown service")]
    UnknownService,
    /// The command is not valid in the service's current state.
    #[error("command is invalid in the service's current state: {0}")]
    InvalidState(String),
    /// The latest config could not be loaded safely before restarting.
    #[error("config reload failed: {0}")]
    ConfigReload(String),
    /// The session policy does not permit the requested operation.
    #[error("policy denied: {0}")]
    PolicyDenied(String),
    /// A configured resource limit would be exceeded.
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
    /// The caller's optimistic-concurrency revision is stale.
    #[error("revision mismatch: expected {expected}, actual {actual}")]
    RevisionMismatch {
        /// Revision supplied by the caller.
        expected: u64,
        /// Current revision in the scheduler.
        actual: u64,
    },
    /// The supplied service definition is invalid.
    #[error("invalid service spec: {0}")]
    InvalidSpec(String),
}

/// The inner result of a service-control command: either the per-service acks, or a typed rejection.
pub type ServiceCommandResult = Result<Vec<ServiceCommandAck>, CommandRejection>;

/// Result of one dynamic-service mutation.
pub type DynamicServiceResult = Result<DynamicServiceAck, CommandRejection>;

/// Kind of semantic change reported by config reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileActionKind {
    /// A configured service will be added or revived.
    Added,
    /// A configured service will be retired.
    Removed,
    /// A configured service definition changed in place.
    Changed,
}

/// One configured-service change reported by config reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReconcileAction {
    /// Affected service.
    pub service: ServiceID,
    /// Semantic change kind.
    pub action: ReconcileActionKind,
    /// Human-readable summary of the changed definition components.
    pub detail: String,
}

/// Scheduler acknowledgement for a config reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReconcileReceipt {
    /// Config file read by the scheduler.
    pub config_path: String,
    /// Whether this invocation only computed the semantic diff.
    pub dry_run: bool,
    /// Changed services, ordered by service id.
    pub actions: Vec<ReconcileAction>,
}

/// Result of one config reconciliation.
pub type ReconcileResult = Result<ReconcileReceipt, CommandRejection>;

/// The scheduler dropped the reply channel (it is shutting down) before acknowledging a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the scheduler has stopped")]
pub struct SchedulerStopped;

/// The reply half of an acknowledged command. Possession-scoped: there is no public constructor, so
/// only the crate (the `ServiceControl` dispatch path) can attach an ack to a `Command`, and only
/// the scheduler can satisfy it.
#[derive(Debug)]
pub struct CommandAck {
    tx: oneshot::Sender<ServiceCommandResult>,
}

/// Reply half for a dynamic-service command.
#[derive(Debug)]
pub struct DynamicCommandAck {
    tx: oneshot::Sender<DynamicServiceResult>,
}

/// Reply half for a config-reconciliation command.
#[derive(Debug)]
pub struct ReconcileCommandAck {
    tx: oneshot::Sender<ReconcileResult>,
}

impl CommandAck {
    pub(crate) fn new() -> (Self, oneshot::Receiver<ServiceCommandResult>) {
        let (tx, rx) = oneshot::channel();
        (Self { tx }, rx)
    }

    /// Satisfy the command. A dropped receiver (caller gone) is not an error here.
    pub(crate) fn send(self, result: ServiceCommandResult) {
        let _ = self.tx.send(result);
    }
}

impl DynamicCommandAck {
    pub(crate) fn new() -> (Self, oneshot::Receiver<DynamicServiceResult>) {
        let (tx, rx) = oneshot::channel();
        (Self { tx }, rx)
    }

    pub(crate) fn send(self, result: DynamicServiceResult) {
        let _ = self.tx.send(result);
    }
}

impl ReconcileCommandAck {
    pub(crate) fn new() -> (Self, oneshot::Receiver<ReconcileResult>) {
        let (tx, rx) = oneshot::channel();
        (Self { tx }, rx)
    }

    pub(crate) fn send(self, result: ReconcileResult) {
        let _ = self.tx.send(result);
    }
}

/// The restricted command port handed to untrusted adapters. Cloning shares one channel to the
/// scheduler.
#[derive(Clone)]
pub struct ServiceControl {
    tx: mpsc::Sender<Command>,
}

impl ServiceControl {
    pub(crate) fn new(tx: mpsc::Sender<Command>) -> Self {
        Self { tx }
    }

    async fn dispatch(
        &self,
        make: impl FnOnce(CommandAck) -> Command,
    ) -> Result<ServiceCommandResult, SchedulerStopped> {
        let (ack, rx) = CommandAck::new();
        self.tx
            .send(make(ack))
            .await
            .map_err(|_| SchedulerStopped)?;
        rx.await.map_err(|_| SchedulerStopped)
    }

    /// Restart a single service.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands. The inner
    /// result is the scheduler's typed acknowledgement or rejection.
    pub async fn restart(&self, id: &ServiceID) -> Result<ServiceCommandResult, SchedulerStopped> {
        let id = id.clone();
        self.dispatch(move |ack| Command::Restart {
            service: id,
            ack: Some(ack),
        })
        .await
    }

    /// Restart all enabled services.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn restart_all(&self) -> Result<ServiceCommandResult, SchedulerStopped> {
        self.dispatch(|ack| Command::RestartAll { ack: Some(ack) })
            .await
    }

    /// Enable (and start) a single service.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn enable(&self, id: &ServiceID) -> Result<ServiceCommandResult, SchedulerStopped> {
        let id = id.clone();
        self.dispatch(move |ack| Command::Enable {
            service: id,
            ack: Some(ack),
        })
        .await
    }

    /// Disable a single service.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn disable(&self, id: &ServiceID) -> Result<ServiceCommandResult, SchedulerStopped> {
        let id = id.clone();
        self.dispatch(move |ack| Command::Disable {
            service: id,
            ack: Some(ack),
        })
        .await
    }

    /// Reconcile configured services against the current on-disk config.
    ///
    /// This operation has no dynamic-service capability gate: the config file is the authority.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn reconcile_config(
        &self,
        dry_run: bool,
    ) -> Result<ReconcileResult, SchedulerStopped> {
        let (ack, rx) = ReconcileCommandAck::new();
        self.tx
            .send(Command::ReconcileConfig { dry_run, ack })
            .await
            .map_err(|_| SchedulerStopped)?;
        rx.await.map_err(|_| SchedulerStopped)
    }

    /// Create and start a dynamic service.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn start_dynamic(
        &self,
        params: DynamicServiceParams,
    ) -> Result<DynamicServiceResult, SchedulerStopped> {
        let (ack, rx) = DynamicCommandAck::new();
        self.tx
            .send(Command::StartDynamic {
                params,
                ack: Some(ack),
            })
            .await
            .map_err(|_| SchedulerStopped)?;
        rx.await.map_err(|_| SchedulerStopped)
    }

    /// Replace or revive a dynamic service at an expected revision.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn replace_dynamic(
        &self,
        service: &ServiceID,
        expected_revision: u64,
        params: DynamicServiceParams,
    ) -> Result<DynamicServiceResult, SchedulerStopped> {
        let (ack, rx) = DynamicCommandAck::new();
        self.tx
            .send(Command::ReplaceDynamic {
                service: service.clone(),
                expected_revision,
                params,
                ack: Some(ack),
            })
            .await
            .map_err(|_| SchedulerStopped)?;
        rx.await.map_err(|_| SchedulerStopped)
    }

    /// Renew a live dynamic service's lease without restarting it.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn renew_dynamic(
        &self,
        service: &ServiceID,
        expected_revision: u64,
        expires_after: Option<Lease>,
    ) -> Result<DynamicServiceResult, SchedulerStopped> {
        let (ack, rx) = DynamicCommandAck::new();
        self.tx
            .send(Command::RenewDynamic {
                service: service.clone(),
                expected_revision,
                expires_after,
                ack: Some(ack),
            })
            .await
            .map_err(|_| SchedulerStopped)?;
        rx.await.map_err(|_| SchedulerStopped)
    }

    /// Retire a dynamic service without removing its post-mortem state.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn stop_dynamic(
        &self,
        service: &ServiceID,
    ) -> Result<DynamicServiceResult, SchedulerStopped> {
        let (ack, rx) = DynamicCommandAck::new();
        self.tx
            .send(Command::StopDynamic {
                service: service.clone(),
                ack: Some(ack),
            })
            .await
            .map_err(|_| SchedulerStopped)?;
        rx.await.map_err(|_| SchedulerStopped)
    }
}
