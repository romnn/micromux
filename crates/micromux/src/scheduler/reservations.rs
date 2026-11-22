//! The scheduler's side of resource reservations: which resources this session holds, waits
//! for, or releases, who may control the services that need them, and the store transactions
//! behind each change.
//!
//! The store records who holds a resource across sessions; this side decides when this session
//! starts, stops, and gives up its own runs.
//! A grant's authority ends at its deadline, taken from the store's `renew_by` on the monotonic
//! clock when the grant is made, renewed, or noticed after a promotion.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use tokio::sync::mpsc;

use super::{
    Caller, CommandRejection, ProcessEvent, ReservationRequest, SchedulerRuntime, ServiceEventKind,
    ServiceID, unix_now_ms,
};
use crate::ServiceMap;
use crate::session::{
    Request, ReservationError, ReservationReply, ReservationState, ResourceStore,
};

/// How often the scheduler checks on a queued request and on a release in progress.
pub(super) const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Where this session's request for one resource stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mine {
    /// The request waits in the queue.
    Waiting,
    /// The session may start the resource's services until `deadline`.
    Granted { deadline: tokio::time::Instant },
    /// The session stops its services that need the resource, then gives it up.
    Draining {
        /// Whether giving it up failed once, so only the poll timer retries it.
        removal_pending: bool,
    },
}

impl Mine {
    /// Whether the session may start a service of the resource now: it holds the grant and the
    /// grant's deadline has not passed.
    pub(super) fn may_start(self) -> bool {
        matches!(self, Self::Granted { deadline } if deadline > tokio::time::Instant::now())
    }
}

/// The monotonic deadline of a grant that lapses at `renew_by_unix_ms`.
fn deadline(renew_by_unix_ms: u64) -> tokio::time::Instant {
    let remaining = renew_by_unix_ms.saturating_sub(unix_now_ms().unwrap_or_default());
    tokio::time::Instant::now() + Duration::from_millis(remaining)
}

/// The command rejection that reports `err`.
fn rejection(err: &ReservationError) -> CommandRejection {
    let message = err.to_string();
    match err {
        ReservationError::InvalidName { .. } | ReservationError::ReasonTooLong => {
            CommandRejection::InvalidSpec(message)
        }
        ReservationError::QueueFull { .. } => CommandRejection::LimitExceeded(message),
        ReservationError::Gone { .. } => CommandRejection::InvalidState(message),
        ReservationError::Registry(_)
        | ReservationError::Io { .. }
        | ReservationError::Unreadable { .. } => CommandRejection::ReservationStore(message),
        ReservationError::Held { .. }
        | ReservationError::Queued { .. }
        | ReservationError::Waiting { .. }
        | ReservationError::Stopping { .. }
        | ReservationError::Draining { .. }
        | ReservationError::Lapsed { .. } => CommandRejection::ResourceUnavailable(message),
    }
}

/// The resource of a dynamic service's replacement: the service's own, or the one its
/// `from_service` clone brings.
pub(super) fn replacement_resource(
    service_id: &str,
    cloned: Option<String>,
    existing: Option<String>,
) -> Result<Option<String>, CommandRejection> {
    match (cloned, existing) {
        (Some(cloned), Some(existing)) if cloned != existing => {
            Err(CommandRejection::InvalidSpec(format!(
                "service `{service_id}` needs resource `{existing}`, but its `from_service` needs \
                 `{cloned}`"
            )))
        }
        (cloned, existing) => Ok(cloned.or(existing)),
    }
}

impl SchedulerRuntime {
    /// Whether `caller` is this session's own process or project.
    pub(super) fn is_own(&self, caller: &Caller) -> bool {
        match caller {
            Caller::Local => true,
            Caller::Project(id) => self
                .session
                .resources
                .as_ref()
                .is_some_and(|store| store.session_id() == id),
            Caller::Unknown => false,
        }
    }

    /// The project that may control this session's services that need a resource.
    fn own_project(&self) -> String {
        self.session.resources.as_ref().map_or_else(
            || "this session's own process".to_string(),
            |store| format!("the project of {}", store.config_path()),
        )
    }

    /// Refuses `caller` from another project when `service` needs `resource`.
    pub(super) fn require_own(
        &self,
        caller: &Caller,
        service: &str,
        resource: Option<&str>,
    ) -> Result<(), CommandRejection> {
        match resource {
            Some(resource) if !self.is_own(caller) => Err(CommandRejection::PolicyDenied(format!(
                "service `{service}` needs resource `{resource}`, so only {} may control it",
                self.own_project()
            ))),
            _ => Ok(()),
        }
    }

    /// The resource that makes `service` protected: the one its live run captured, else the one
    /// its current definition names.
    ///
    /// During a replacement the run and the definition can differ, and either one protects it.
    pub(super) fn resource_of(&self, services: &ServiceMap, service: &ServiceID) -> Option<String> {
        self.services
            .get(service)
            .and_then(|runtime| runtime.running.as_ref())
            .and_then(|running| running.resource.clone())
            .or_else(|| {
                services
                    .get(service)
                    .and_then(|service| service.spec.resource.clone())
            })
    }

    /// Runs `op` on the reservation store off the scheduler task, handling process events
    /// meanwhile.
    pub(super) async fn store_op<T: Send + 'static>(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
        op: impl FnOnce(&ResourceStore) -> Result<T, ReservationError> + Send + 'static,
    ) -> Result<T, CommandRejection> {
        let Some(store) = self.session.resources.clone() else {
            return Err(CommandRejection::ResourceUnavailable(
                "the session has no config file, so it cannot hold resources".to_string(),
            ));
        };
        let task = async move {
            tokio::task::spawn_blocking(move || op(&store))
                .await
                .map_err(|err| format!("reservation task failed: {err}"))
        };
        self.await_loaded_services(services, events_rx, task)
            .await
            .map_err(CommandRejection::ReservationStore)?
            .map_err(|err| rejection(&err))
    }

    /// Refuses a command from another project that would start `service` with its definition,
    /// which may differ from the one in `services`, when the live run or either definition
    /// names a resource.
    pub(super) fn require_own_start(
        &self,
        services: &ServiceMap,
        caller: &Caller,
        service: &crate::service::Service,
    ) -> Result<(), CommandRejection> {
        let protected = self
            .resource_of(services, &service.id)
            .or_else(|| service.spec.resource.clone());
        self.require_own(caller, &service.id, protected.as_deref())
    }

    /// Lets this session start services of `resource`: a grant it holds is used as it is, and
    /// otherwise the store grants the resource, or reports the grant a promotion gave this
    /// session.
    ///
    /// A valid grant needs no store transaction, and its deadline stays as it is. A claim this
    /// session is releasing or let lapse stays ended, whatever the store says.
    pub(super) async fn admit(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
        resource: &str,
    ) -> Result<(), CommandRejection> {
        if let Some(ended) = self.claim_ended(resource) {
            return Err(rejection(&ended));
        }
        if self.mine.get(resource).is_some_and(|mine| mine.may_start()) {
            return Ok(());
        }
        let name = resource.to_string();
        let renew_by = self
            .store_op(services, events_rx, move |store| store.admit(&name))
            .await?;
        self.mine.insert(
            resource.to_string(),
            Mine::Granted {
                deadline: deadline(renew_by),
            },
        );
        Ok(())
    }

    fn disable_for_resource(&mut self, services: &ServiceMap, service_id: &ServiceID, why: &str) {
        let Some(runtime) = self.services.get_mut(service_id) else {
            return;
        };
        runtime.disable();
        self.sync(services, service_id);
        self.append_event(service_id, ServiceEventKind::DisableRequested, why);
    }

    /// Whether a live run of this session, retired services included, captured `resource`.
    fn runs_resource(&self, resource: &str) -> bool {
        self.services.values().any(|runtime| {
            runtime
                .running
                .as_ref()
                .is_some_and(|running| running.resource.as_deref() == Some(resource))
        })
    }

    /// Starts giving up `resource`: disables every service that needs it, so its runs stop, and
    /// marks the grant as draining.
    ///
    /// The runs stop even when the store cannot be written: the session holds no grant once it
    /// decided so.
    pub(super) async fn begin_drain(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
        resource: &str,
        detail: &str,
    ) {
        self.mine.insert(
            resource.to_string(),
            Mine::Draining {
                removal_pending: false,
            },
        );
        let affected = services
            .iter()
            .filter(|(service_id, service)| {
                service.spec.resource.as_deref() == Some(resource)
                    || self
                        .services
                        .get(*service_id)
                        .and_then(|runtime| runtime.running.as_ref())
                        .is_some_and(|running| running.resource.as_deref() == Some(resource))
            })
            .map(|(service_id, _)| service_id.clone())
            .collect::<Vec<_>>();
        for service_id in &affected {
            self.disable_for_resource(services, service_id, detail);
        }
        let name = resource.to_string();
        if let Err(err) = self
            .store_op(services, events_rx, move |store| store.begin_drain(&name))
            .await
        {
            tracing::warn!(%err, resource, "cannot mark the reservation as draining");
        }
    }

    /// Gives up every draining resource none of whose runs is left.
    ///
    /// A resource whose removal failed before is retried only with `retry_pending`.
    pub(super) async fn settle_drains(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
        retry_pending: bool,
    ) {
        let done = self
            .mine
            .iter()
            .filter(|(resource, mine)| {
                matches!(mine, Mine::Draining { removal_pending } if retry_pending || !removal_pending)
                    && !self.runs_resource(resource)
            })
            .map(|(resource, _)| resource.clone())
            .collect::<Vec<_>>();
        for resource in done {
            let name = resource.clone();
            match self
                .store_op(services, events_rx, move |store| store.finish_drain(&name))
                .await
            {
                Ok(()) => {
                    self.mine.remove(&resource);
                }
                Err(err) => {
                    tracing::warn!(%err, resource, "cannot release the reservation; retrying");
                    self.mine.insert(
                        resource,
                        Mine::Draining {
                            removal_pending: true,
                        },
                    );
                }
            }
        }
    }

    /// The earliest deadline of a grant, when this session holds any.
    pub(super) fn next_lapse(&self) -> Option<tokio::time::Instant> {
        self.mine
            .values()
            .filter_map(|mine| match mine {
                Mine::Granted { deadline } => Some(*deadline),
                Mine::Waiting | Mine::Draining { .. } => None,
            })
            .min()
    }

    /// Starts giving up every grant past its deadline.
    pub(super) async fn lapse_due(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
    ) {
        let now = tokio::time::Instant::now();
        let due = self
            .mine
            .iter()
            .filter(|(_, mine)| matches!(mine, Mine::Granted { deadline } if *deadline <= now))
            .map(|(resource, _)| resource.clone())
            .collect::<Vec<_>>();
        for resource in due {
            let detail = format!(
                "service disabled because this session's reservation of resource `{resource}` \
                 lapsed"
            );
            self.begin_drain(services, events_rx, &resource, &detail)
                .await;
        }
    }

    /// Whether a queued request or a release needs the poll timer.
    pub(super) fn needs_poll(&self) -> bool {
        self.mine
            .values()
            .any(|mine| matches!(mine, Mine::Waiting | Mine::Draining { .. }))
    }

    /// Finishes releases whose runs are gone and notices queued requests that were granted or
    /// dropped.
    pub(super) async fn poll_reservations(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
    ) {
        self.settle_drains(services, events_rx, true).await;
        let waiting = self
            .mine
            .iter()
            .filter(|(_, mine)| matches!(mine, Mine::Waiting))
            .map(|(resource, _)| resource.clone())
            .collect::<Vec<_>>();
        for resource in waiting {
            let name = resource.clone();
            match self
                .store_op(services, events_rx, move |store| store.poll(&name))
                .await
            {
                Ok(reply) => self.track(&resource, &reply),
                Err(err) => tracing::warn!(%err, resource, "cannot poll the reservation"),
            }
        }
    }

    /// Follows where this session's request stands after a store operation.
    ///
    /// The caller made sure the claim had not ended before the operation, and nothing ends it
    /// while the scheduler awaits the store, so a renewal the store committed always moves the
    /// deadline.
    fn track(&mut self, resource: &str, reply: &ReservationReply) {
        let mine = match (reply.state, reply.own_claim()) {
            (ReservationState::Granted, Some(claim)) => Mine::Granted {
                deadline: deadline(claim.renew_by_unix_ms),
            },
            (ReservationState::Waiting { .. }, _) => Mine::Waiting,
            (ReservationState::Draining, _) => Mine::Draining {
                removal_pending: false,
            },
            (ReservationState::Released | ReservationState::Granted, _) => {
                self.mine.remove(resource);
                return;
            }
        };
        self.mine.insert(resource.to_string(), mine);
    }

    /// Why this session's claim of `resource` can no longer be renewed, if it is releasing or
    /// past its deadline.
    fn claim_ended(&self, resource: &str) -> Option<ReservationError> {
        let resource_name = resource.to_string();
        match self.mine.get(resource) {
            Some(Mine::Draining { .. }) => Some(ReservationError::Draining {
                resource: resource_name,
            }),
            Some(Mine::Granted { deadline }) if *deadline <= tokio::time::Instant::now() => {
                Some(ReservationError::Lapsed {
                    resource: resource_name,
                })
            }
            Some(Mine::Granted { .. } | Mine::Waiting) | None => None,
        }
    }

    /// Applies `request` for `caller`, which must be the session's own process or project.
    pub(super) async fn reservation(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
        caller: &Caller,
        request: ReservationRequest,
    ) -> Result<ReservationReply, CommandRejection> {
        if !self.is_own(caller) {
            return Err(CommandRejection::PolicyDenied(format!(
                "only {} may reserve, renew, or release resources for this session",
                self.own_project()
            )));
        }
        match request {
            ReservationRequest::Reserve {
                resource,
                estimate_secs,
                reason,
            } => {
                if let Some(ended) = self.claim_ended(&resource) {
                    return Err(rejection(&ended));
                }
                let request = Request {
                    estimate_secs,
                    reason,
                };
                let name = resource.clone();
                let reply = self
                    .store_op(services, events_rx, move |store| {
                        store.reserve(&name, request)
                    })
                    .await?;
                self.track(&resource, &reply);
                Ok(reply)
            }
            ReservationRequest::Renew { resource, claim } => {
                if let Some(ended) = self.claim_ended(&resource) {
                    return Err(rejection(&ended));
                }
                let name = resource.clone();
                let reply = self
                    .store_op(services, events_rx, move |store| {
                        store.renew(&name, claim.as_deref())
                    })
                    .await?;
                self.track(&resource, &reply);
                Ok(reply)
            }
            ReservationRequest::Release { resource } => {
                self.release(services, events_rx, resource).await
            }
        }
    }

    /// Releases this session's request for `resource`: a queued request goes at once, a grant
    /// once the services that need it stopped.
    async fn release(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
        resource: String,
    ) -> Result<ReservationReply, CommandRejection> {
        match self.mine.get(&resource) {
            Some(Mine::Granted { .. }) => {
                let detail =
                    format!("service disabled because this session released resource `{resource}`");
                self.begin_drain(services, events_rx, &resource, &detail)
                    .await;
                self.settle_drains(services, events_rx, false).await;
            }
            Some(Mine::Draining { .. }) => {}
            Some(Mine::Waiting) | None => {
                let name = resource.clone();
                self.store_op(services, events_rx, move |store| store.finish_drain(&name))
                    .await?;
                self.mine.remove(&resource);
            }
        }
        let name = resource.clone();
        self.store_op(services, events_rx, move |store| store.poll(&name))
            .await
    }

    /// Whether `caller` may shut the session down: another project may not while the session
    /// holds or waits for a resource, or runs or configures a service that needs one.
    pub(super) fn authorize_shutdown(
        &self,
        services: &ServiceMap,
        caller: &Caller,
    ) -> Result<(), CommandRejection> {
        if self.is_own(caller) {
            return Ok(());
        }
        let resources = self.watched_resources(services);
        if resources.is_empty() {
            return Ok(());
        }
        Err(CommandRejection::PolicyDenied(format!(
            "the session uses resources {}, so only {} may stop it",
            resources.into_iter().collect::<Vec<_>>().join(", "),
            self.own_project()
        )))
    }

    /// The resources this session holds, waits for, runs, or names in a live service.
    pub(super) fn watched_resources(&self, services: &ServiceMap) -> BTreeSet<String> {
        let named = services.iter().filter_map(|(service_id, service)| {
            let runtime = self.services.get(service_id)?;
            let running = runtime
                .running
                .as_ref()
                .and_then(|running| running.resource.clone());
            running.or_else(|| {
                runtime
                    .is_live()
                    .then(|| service.spec.resource.clone())
                    .flatten()
            })
        });
        self.mine.keys().cloned().chain(named).collect()
    }

    /// Admits the resources that services ready to start on their own need, at most once each,
    /// and schedules again after every grant, until nothing new is needed.
    ///
    /// A service refused its resource is disabled with an event naming the holder.
    pub(super) async fn schedule_with_admission(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
    ) {
        let mut attempted = BTreeSet::new();
        loop {
            let mut needed = BTreeMap::<String, Vec<ServiceID>>::new();
            super::schedule::schedule_ready(&mut super::schedule::ScheduleContext {
                services,
                runtimes: &mut self.services,
                current_pty_size: self.current_pty_size,
                events_tx: &self.events_tx,
                #[cfg(test)]
                test_events: &mut self.test_events,
                writer: &self.writer,
                shutdown: &self.shutdown,
                resources: self.session.resources.as_ref(),
                mine: &self.mine,
                needed_resources: &mut needed,
            });
            needed.retain(|resource, _| !attempted.contains(resource));
            if needed.is_empty() || self.shutdown.is_cancelled() {
                break;
            }
            for (resource, waiting) in needed {
                if let Err(rejection) = self.admit(services, events_rx, &resource).await {
                    for service_id in &waiting {
                        self.disable_for_resource(
                            services,
                            service_id,
                            &format!("service disabled because {rejection}"),
                        );
                    }
                }
                attempted.insert(resource);
            }
        }
        self.session_status
            .set_watched_resources(self.watched_resources(services));
    }
}
