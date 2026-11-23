//! The narrow command capability handed to untrusted adapters (control server, MCP).
//!
//! [`ServiceControl`] exposes only the safe service operations and is **request/response**: each
//! method carries a oneshot reply, so the *scheduler* validates the command and latches the
//! generation at the exact moment it processes it — there is no pre-queue snapshot race. The type
//! has no `send_input`/`resize_all` method and cannot construct those `Command` variants, so raw
//! input forwarding is *unreachable* through it; the trusted in-process TUI keeps the full
//! `mpsc::Sender<Command>` instead.
//! PTY input and resizes travel only through the separate, bounded [`crate::TerminalControl`],
//! which the in-process TUI and the control server (for attached TUIs) hold.
//! The MCP server never sends them.

use super::types::{Command, ReservationRequest, ServiceID};
use crate::session::ReservationReply;
use crate::{DynamicServiceParams, Lease, RestartPolicy, ServiceSpec};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tokio::sync::{mpsc, oneshot};

/// Who sent a command, which decides whether it may touch services that need a resource.
///
/// The scope is cooperative: every session of a user can claim any project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// The session's own process, such as its in-process TUI.
    Local,
    /// A client that named its home project by session id, the digest of the project's
    /// canonical config path.
    Project(String),
    /// A client that did not name its home project.
    Unknown,
}

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
    /// Environment key names.
    ///
    /// Values leave the scheduler only through [`ServiceControl::environment`].
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

/// Environment variables as the scheduler resolved them, for the session or for one service.
///
/// Reused directly as the control-protocol wire payload, like [`ServiceCommandAck`].
/// Values may hold secrets from env files, so the [`Debug`] output names the variables but never
/// shows their values.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResolvedEnvironment {
    /// The variables by name.
    pub variables: BTreeMap<String, String>,
    /// Whether the service's definition changed since its running process started.
    ///
    /// The variables are then those of the running process, not the ones its next run gets.
    /// Always `false` for the session's variables, which are fixed while the session runs, and
    /// for its own environment, which is resolved anew for every query.
    pub stale: bool,
}

impl std::fmt::Debug for ResolvedEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedEnvironment")
            .field("variables", &self.variables.keys().collect::<Vec<_>>())
            .field("stale", &self.stale)
            .finish()
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
    /// A resource the command needs is held or queued for by another session, or this session's
    /// claim of it is gone, releasing, or lapsed.
    #[error("resource unavailable: {0}")]
    ResourceUnavailable(String),
    /// The reservation store could not be read or written, so nobody can tell who holds the
    /// resource; trying again may succeed.
    #[error("reservation store failed: {0}")]
    ReservationStore(String),
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
    /// A configured service definition or its relative position changed in place.
    Changed,
}

/// One configured-service change reported by config reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReconcileAction {
    /// Affected service.
    pub service: ServiceID,
    /// Semantic change kind.
    pub action: ReconcileActionKind,
    /// Human-readable summary of the changed definition components or service order.
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

/// Result of one environment query.
pub type EnvironmentResult = Result<ResolvedEnvironment, CommandRejection>;

impl CommandRejection {
    /// What went wrong, without the kind of rejection in front, for a sentence of its own.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::InvalidState(message)
            | Self::ConfigReload(message)
            | Self::PolicyDenied(message)
            | Self::LimitExceeded(message)
            | Self::InvalidSpec(message)
            | Self::ResourceUnavailable(message)
            | Self::ReservationStore(message) => message.clone(),
            Self::UnknownService | Self::RevisionMismatch { .. } => self.to_string(),
        }
    }

    /// The same rejection with `note` appended to its message; a rejection without a message
    /// stays as it is.
    #[must_use]
    pub(crate) fn with_note(self, note: &str) -> Self {
        let noted = |message: String| format!("{message}; {note}");
        match self {
            Self::InvalidState(message) => Self::InvalidState(noted(message)),
            Self::ConfigReload(message) => Self::ConfigReload(noted(message)),
            Self::PolicyDenied(message) => Self::PolicyDenied(noted(message)),
            Self::LimitExceeded(message) => Self::LimitExceeded(noted(message)),
            Self::InvalidSpec(message) => Self::InvalidSpec(noted(message)),
            Self::ResourceUnavailable(message) => Self::ResourceUnavailable(noted(message)),
            Self::ReservationStore(message) => Self::ReservationStore(noted(message)),
            Self::UnknownService | Self::RevisionMismatch { .. } => self,
        }
    }
}

/// Result of one reserve, renew or release.
pub type ReservationResult = Result<ReservationReply, CommandRejection>;

/// The scheduler dropped the reply channel (it is shutting down) before acknowledging a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the scheduler has stopped")]
pub struct SchedulerStopped;

/// The reply half of an acknowledged command, answered with `Result<T, CommandRejection>`.
///
/// Possession-scoped: there is no public constructor, so only the crate (the `ServiceControl`
/// dispatch path) can attach an ack to a `Command`, and only the scheduler can satisfy it.
#[derive(Debug)]
pub struct ReplyAck<T> {
    tx: oneshot::Sender<Result<T, CommandRejection>>,
}

impl<T> ReplyAck<T> {
    pub(crate) fn new() -> (Self, oneshot::Receiver<Result<T, CommandRejection>>) {
        let (tx, rx) = oneshot::channel();
        (Self { tx }, rx)
    }

    /// Satisfy the command. A dropped receiver (caller gone) is not an error here.
    pub(crate) fn send(self, result: Result<T, CommandRejection>) {
        let _ = self.tx.send(result);
    }
}

impl From<mpsc::Sender<Command>> for ServiceControl {
    fn from(tx: mpsc::Sender<Command>) -> Self {
        Self::new(tx)
    }
}

/// The restricted command port handed to untrusted adapters. Cloning shares one channel to the
/// scheduler.
///
/// Every mutation names its [`Caller`]: only the session's own process or project may control a
/// service that needs a resource, which the scheduler refuses for anyone else with
/// [`CommandRejection::PolicyDenied`].
#[derive(Clone)]
pub struct ServiceControl {
    tx: mpsc::Sender<Command>,
}

impl ServiceControl {
    pub(crate) fn new(tx: mpsc::Sender<Command>) -> Self {
        Self { tx }
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(ReplyAck<T>) -> Command,
    ) -> Result<Result<T, CommandRejection>, SchedulerStopped> {
        let (ack, rx) = ReplyAck::new();
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
    pub async fn restart(
        &self,
        caller: Caller,
        id: &ServiceID,
    ) -> Result<ServiceCommandResult, SchedulerStopped> {
        let id = id.clone();
        self.request(move |ack| Command::Restart {
            caller,
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
    pub async fn restart_all(
        &self,
        caller: Caller,
    ) -> Result<ServiceCommandResult, SchedulerStopped> {
        self.request(|ack| Command::RestartAll {
            caller,
            ack: Some(ack),
        })
        .await
    }

    /// Enable (and start) a single service.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn enable(
        &self,
        caller: Caller,
        id: &ServiceID,
    ) -> Result<ServiceCommandResult, SchedulerStopped> {
        let id = id.clone();
        self.request(move |ack| Command::Enable {
            caller,
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
    pub async fn disable(
        &self,
        caller: Caller,
        id: &ServiceID,
    ) -> Result<ServiceCommandResult, SchedulerStopped> {
        let id = id.clone();
        self.request(move |ack| Command::Disable {
            caller,
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
        caller: Caller,
        dry_run: bool,
    ) -> Result<ReconcileResult, SchedulerStopped> {
        self.request(|ack| Command::ReconcileConfig {
            caller,
            dry_run,
            ack,
        })
        .await
    }

    /// Report the variables the session injects, or the environment it gives one service.
    ///
    /// With `None`, these are the session's own variables: its name, domain, proxy port, host
    /// suffix, and allocated ports; [`Self::session_environment`] adds the session-wide
    /// `env_file`.
    /// With a service id, they are that service's resolved session-wide `env_file`, own
    /// `env_file`, and `environment` entries with the session's variables applied, as its live
    /// run got them, or as its next run gets them when no run is live.
    /// The variables micromux sets on every spawn itself, such as `TERM`, are not included.
    /// The id must match exactly; a display name does not select a service.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    /// The inner result rejects an unknown service with [`CommandRejection::UnknownService`].
    pub async fn environment(
        &self,
        service: Option<&ServiceID>,
    ) -> Result<EnvironmentResult, SchedulerStopped> {
        self.request(|ack| Command::Environment {
            service: service.cloned(),
            ack,
        })
        .await
    }

    /// Report the session's own environment: the session-wide `env_file`, read from disk for
    /// this query with the resolver services use, and the session's variables over it.
    ///
    /// Values may be secrets from env files, unlike the variables alone that
    /// [`Self::environment`] reports without a service.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    /// The inner result rejects a session-wide `env_file` that cannot be loaded with
    /// [`CommandRejection::ConfigReload`].
    pub async fn session_environment(&self) -> Result<EnvironmentResult, SchedulerStopped> {
        self.request(|ack| Command::SessionEnvironment { ack })
            .await
    }

    /// Create and start a dynamic service.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn start_dynamic(
        &self,
        caller: Caller,
        params: DynamicServiceParams,
    ) -> Result<DynamicServiceResult, SchedulerStopped> {
        self.request(|ack| Command::StartDynamic {
            caller,
            params,
            ack: Some(ack),
        })
        .await
    }

    /// Replace or revive a dynamic service at an expected revision.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn replace_dynamic(
        &self,
        caller: Caller,
        service: &ServiceID,
        expected_revision: u64,
        params: DynamicServiceParams,
    ) -> Result<DynamicServiceResult, SchedulerStopped> {
        self.request(|ack| Command::ReplaceDynamic {
            caller,
            service: service.clone(),
            expected_revision,
            params,
            ack: Some(ack),
        })
        .await
    }

    /// Renew a live dynamic service's lease without restarting it.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn renew_dynamic(
        &self,
        caller: Caller,
        service: &ServiceID,
        expected_revision: u64,
        expires_after: Option<Lease>,
    ) -> Result<DynamicServiceResult, SchedulerStopped> {
        self.request(|ack| Command::RenewDynamic {
            caller,
            service: service.clone(),
            expected_revision,
            expires_after,
            ack: Some(ack),
        })
        .await
    }

    /// Retire a dynamic service without removing its post-mortem state.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn stop_dynamic(
        &self,
        caller: Caller,
        service: &ServiceID,
    ) -> Result<DynamicServiceResult, SchedulerStopped> {
        self.request(|ack| Command::StopDynamic {
            caller,
            service: service.clone(),
            ack: Some(ack),
        })
        .await
    }

    /// Reserve `resource` for this session: granted at once while it is free, queued otherwise.
    ///
    /// A fresh request also requests a start of the session's configured services that name the
    /// resource and start with the session, which then run as soon as the session gets it.
    /// A request this session already has is updated and renewed, and enables nothing.
    /// `estimate_secs` and `reason` are shown to every other session and change nothing about
    /// the grant.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    /// The inner result rejects a caller from another project with
    /// [`CommandRejection::PolicyDenied`].
    pub async fn reserve(
        &self,
        caller: Caller,
        resource: &str,
        estimate_secs: Option<u64>,
        reason: Option<String>,
    ) -> Result<ReservationResult, SchedulerStopped> {
        let request = ReservationRequest::Reserve {
            resource: resource.to_string(),
            estimate_secs,
            reason,
        };
        self.reservation(caller, request).await
    }

    /// Move the `renew_by` deadline of this session's request for `resource`, granted or queued.
    ///
    /// With `claim`, only that request is renewed, so a waiter notices that its request was
    /// replaced.
    /// A grant past its deadline while another request waits is being handed over and cannot be
    /// renewed.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn renew_reservation(
        &self,
        caller: Caller,
        resource: &str,
        claim: Option<String>,
    ) -> Result<ReservationResult, SchedulerStopped> {
        let request = ReservationRequest::Renew {
            resource: resource.to_string(),
            claim,
        };
        self.reservation(caller, request).await
    }

    /// Release this session's request for `resource`.
    ///
    /// A queued request is dropped at once, and the starts that waited for it with it.
    /// A grant becomes due at once: while no other request waits, the session keeps it and its
    /// services keep running, and the reply says `granted`; once one waits, the session stops and
    /// disables its services that need the resource, and the reply says `releasing` until they
    /// have stopped and the resource passes on.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn release_reservation(
        &self,
        caller: Caller,
        resource: &str,
    ) -> Result<ReservationResult, SchedulerStopped> {
        let request = ReservationRequest::Release {
            resource: resource.to_string(),
        };
        self.reservation(caller, request).await
    }

    /// Report where this session's request for `resource` stands, as the reservation file was
    /// last written, without renewing a queued request or extending a grant.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    pub async fn reservation_status(
        &self,
        caller: Caller,
        resource: &str,
    ) -> Result<ReservationResult, SchedulerStopped> {
        let request = ReservationRequest::Status {
            resource: resource.to_string(),
        };
        self.reservation(caller, request).await
    }

    async fn reservation(
        &self,
        caller: Caller,
        request: ReservationRequest,
    ) -> Result<ReservationResult, SchedulerStopped> {
        self.request(move |ack| Command::Reservation {
            caller,
            request,
            ack,
        })
        .await
    }

    /// Stops the session for `caller`: the scheduler cancels the session before it replies, so
    /// it admits and starts nothing more.
    ///
    /// A caller from another project may not while the session holds, waits for, or releases a
    /// resource, or a run of a service that needs one has not ended.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerStopped`] if the scheduler is no longer accepting commands.
    /// The inner result refuses with [`CommandRejection::PolicyDenied`] naming the resources.
    pub async fn shutdown(
        &self,
        caller: Caller,
    ) -> Result<Result<(), CommandRejection>, SchedulerStopped> {
        self.request(move |ack| Command::Shutdown { caller, ack })
            .await
    }
}
