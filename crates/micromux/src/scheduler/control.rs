//! The narrow command capability handed to untrusted adapters (control server, MCP).
//!
//! [`ServiceControl`] exposes only the safe service operations and is **request/response**: each
//! method carries a oneshot reply, so the *scheduler* validates the command and latches the
//! generation at the exact moment it processes it — there is no pre-queue snapshot race. The type
//! has no `send_input`/`resize_all` method and cannot construct those `Command` variants, so raw
//! input forwarding is *unreachable* for adapters; the trusted in-process TUI keeps the full
//! `mpsc::Sender<Command>` instead.

use super::types::{Command, ServiceID};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

/// Acknowledgement for one affected service: the generation the scheduler latched while processing
/// the command. Reused directly as the control-protocol wire payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceCommandAck {
    /// The affected service.
    pub service: ServiceID,
    /// The run generation latched at command-processing time (the generation *before* a
    /// restart/enable action; informational for disable).
    pub observed_generation: u64,
}

/// A typed rejection produced by the scheduler when it declines a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandRejection {
    /// No service with the given id exists.
    UnknownService,
    /// The command is not valid in the service's current state (e.g. restart on a disabled service).
    InvalidState,
}

/// The inner result of a service-control command: either the per-service acks, or a typed rejection.
pub type ServiceCommandResult = Result<Vec<ServiceCommandAck>, CommandRejection>;

/// The scheduler dropped the reply channel (it is shutting down) before acknowledging a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerStopped;

impl std::fmt::Display for SchedulerStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the scheduler has stopped")
    }
}

impl std::error::Error for SchedulerStopped {}

/// The reply half of an acknowledged command. Possession-scoped: there is no public constructor, so
/// only the crate (the `ServiceControl` dispatch path) can attach an ack to a `Command`, and only
/// the scheduler can satisfy it.
#[derive(Debug)]
pub struct CommandAck {
    tx: oneshot::Sender<ServiceCommandResult>,
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
}
