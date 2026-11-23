//! The scheduler's side of resource reservations: which resources this session holds, waits
//! for, or releases, who may control the services that need them, and the store transactions
//! behind each change.
//!
//! The store records who holds a resource across sessions and its `renew_by`, the one deadline;
//! this side decides when this session starts, stops, and gives up its own runs.
//! Before the deadline the scheduler only wakes up to look at the wall clock; once the deadline
//! passed it asks the store, which keeps the grant while nobody waits and refuses it once
//! somebody does.
//! See [ADR 6](https://github.com/romnn/micromux/blob/main/adr/0006-shared-resource-reservations.md)
//! for the decisions behind it.

use std::collections::BTreeSet;
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

/// The longest the scheduler sleeps before looking at the wall clock again while a grant is not
/// yet due, so a suspended machine that wakes past the deadline notices within a minute.
const WALL_CLOCK_LOOK: Duration = Duration::from_mins(1);

/// How often the store judges again a grant past its deadline that nobody waited for, so a
/// waiter that arrives later is noticed.
const OVERDUE_REJUDGE: Duration = Duration::from_secs(30);

/// Where this session's claim of one resource stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Standing {
    /// The claim waits in the queue.
    Waiting,
    /// The session holds the resource until `renew_by_unix_ms`, past which it goes on while the
    /// store, which judged it last until `judged_until`, finds nobody waiting.
    Granted {
        renew_by_unix_ms: u64,
        judged_until: tokio::time::Instant,
    },
    /// The session stops its services that need the resource, then removes its claim.
    Releasing,
}

impl Standing {
    /// Whether the session may start a service of the resource now without asking the store.
    pub(super) fn may_start(self) -> bool {
        match self {
            Self::Granted {
                renew_by_unix_ms,
                judged_until,
            } => !overdue(renew_by_unix_ms) || judged_until > tokio::time::Instant::now(),
            Self::Waiting | Self::Releasing => false,
        }
    }

    /// A grant the store just confirmed with `renew_by_unix_ms`.
    fn granted(renew_by_unix_ms: u64) -> Self {
        Self::Granted {
            renew_by_unix_ms,
            judged_until: tokio::time::Instant::now() + OVERDUE_REJUDGE,
        }
    }

    /// When the scheduler must next wake for this grant: to look at the wall clock before its
    /// deadline, or to have the store judge it again after.
    fn wake_at(self) -> Option<tokio::time::Instant> {
        let Self::Granted {
            renew_by_unix_ms,
            judged_until,
        } = self
        else {
            return None;
        };
        let now = unix_now_ms().unwrap_or_default();
        if renew_by_unix_ms > now {
            let remaining = Duration::from_millis(renew_by_unix_ms - now);
            Some(tokio::time::Instant::now() + remaining.min(WALL_CLOCK_LOOK))
        } else {
            Some(judged_until)
        }
    }
}

/// Whether the wall clock is at or past `renew_by_unix_ms`.
fn overdue(renew_by_unix_ms: u64) -> bool {
    unix_now_ms().unwrap_or_default() >= renew_by_unix_ms
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
        | ReservationError::Releasing { .. }
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

    /// Refuses `caller`, which is not the session's own, saying what only the own project may do
    /// and how to become it.
    fn refuse_foreign(&self, caller: &Caller, what: &str) -> CommandRejection {
        let project = self.session.resources.as_ref().map_or_else(
            || "this session's own process".to_string(),
            |store| {
                let config = std::path::Path::new(store.config_path());
                config.parent().unwrap_or(config).display().to_string()
            },
        );
        let how = match caller {
            Caller::Unknown => format!(
                "this connection did not name its project; run a micromux with control protocol \
                 3.21 or newer from inside {project} (a client outside any project names none)"
            ),
            Caller::Local | Caller::Project(_) => format!(
                "the client's working directory decides its project, not the session it \
                 targets; run it from inside {project}"
            ),
        };
        CommandRejection::PolicyDenied(format!("only the project in {project} may {what}; {how}"))
    }

    /// Refuses `caller` from another project when `service` needs `resource`.
    pub(super) fn require_own(
        &self,
        caller: &Caller,
        service: &str,
        resource: Option<&str>,
    ) -> Result<(), CommandRejection> {
        match resource {
            Some(resource) if !self.is_own(caller) => Err(self.refuse_foreign(
                caller,
                &format!("control service `{service}`, which needs resource `{resource}`"),
            )),
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

    /// Refuses a command from another project whose config reload changes a definition that
    /// names a resource or whose live run captured one, as a reconcile of it would be refused.
    pub(super) fn require_own_reload(
        &self,
        services: &ServiceMap,
        caller: &Caller,
        loaded: &ServiceMap,
    ) -> Result<(), CommandRejection> {
        for (service_id, updated) in loaded {
            if services
                .get(service_id)
                .is_some_and(|current| current.spec == updated.spec)
            {
                continue;
            }
            let resource = self
                .resource_of(services, service_id)
                .or_else(|| updated.spec.resource.clone());
            self.require_own(caller, service_id, resource.as_deref())
                .map_err(|refusal| {
                    refusal.with_note(&format!(
                        "refused because this command reloads a changed definition of \
                         `{service_id}`"
                    ))
                })?;
        }
        Ok(())
    }

    /// Runs `op` for `resource` on the reservation store off the scheduler task, handling
    /// process events meanwhile.
    ///
    /// When the store refuses the session's grant because it is past its deadline while others
    /// wait, the session starts releasing the resource.
    pub(super) async fn store_op<T: Send + 'static>(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
        resource: &str,
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
        let result = self
            .await_loaded_services(services, events_rx, task)
            .await
            .map_err(CommandRejection::ReservationStore)?;
        if let Err(err @ ReservationError::Lapsed { .. }) = &result {
            self.begin_release(services, resource, &format!("service disabled: {err}"));
        }
        result.map_err(|err| rejection(&err))
    }

    /// Lets this session start services of `resource`: a grant it holds is used as it is, and
    /// otherwise the store judges the grant, or grants the free resource with `reason` saying
    /// why.
    ///
    /// A grant before its check needs no store transaction. A claim this session is releasing
    /// stays released, whatever the store says.
    pub(super) async fn admit(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
        resource: &str,
        reason: String,
    ) -> Result<(), CommandRejection> {
        if let Some(refusal) = self.releasing_refusal(resource) {
            return Err(refusal);
        }
        if self
            .claims
            .get(resource)
            .is_some_and(|standing| standing.may_start())
        {
            return Ok(());
        }
        let name = resource.to_string();
        let request = Request {
            estimate_secs: None,
            reason: Some(reason),
        };
        let renew_by = self
            .store_op(services, events_rx, resource, move |store| {
                store.admit(&name, request)
            })
            .await?;
        self.claims
            .insert(resource.to_string(), Standing::granted(renew_by));
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

    /// Starts releasing `resource`: refuses further starts and renewals, and disables every
    /// service that needs it, with `detail` as the event, so its runs stop.
    ///
    /// The claim stays in the store until the runs are gone, see [`Self::settle_releases`].
    fn begin_release(&mut self, services: &ServiceMap, resource: &str, detail: &str) {
        self.claims
            .insert(resource.to_string(), Standing::Releasing);
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
            if self
                .services
                .get(service_id)
                .is_some_and(|runtime| runtime.desired != super::DesiredState::Disabled)
            {
                self.disable_for_resource(services, service_id, detail);
            }
        }
    }

    /// Removes the claim of every resource this session releases whose runs are gone.
    ///
    /// A removal that fails keeps the resource releasing, so the next poll retries it.
    pub(super) async fn settle_releases(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
    ) {
        let done = self
            .claims
            .iter()
            .filter(|(resource, standing)| {
                **standing == Standing::Releasing && !self.runs_resource(resource)
            })
            .map(|(resource, _)| resource.clone())
            .collect::<Vec<_>>();
        for resource in done {
            let name = resource.clone();
            match self
                .store_op(services, events_rx, &resource, move |store| {
                    store.remove(&name)
                })
                .await
            {
                Ok(()) => {
                    self.claims.remove(&resource);
                }
                Err(err) => {
                    tracing::warn!(%err, resource, "cannot release the reservation; retrying");
                }
            }
        }
    }

    /// When the scheduler must next wake for a grant, if this session holds any.
    pub(super) fn next_grant_check(&self) -> Option<tokio::time::Instant> {
        self.claims
            .values()
            .filter_map(|standing| standing.wake_at())
            .min()
    }

    /// Has the store judge every grant past its deadline whose last judgement ran out: it keeps
    /// one that nobody waits for and refuses one that others wait for, which the session then
    /// releases. A grant before its deadline needs no store transaction.
    ///
    /// Returns whether the store judged any grant.
    pub(super) async fn check_due_grants(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
    ) -> bool {
        let due = self
            .claims
            .iter()
            .filter(|(_, standing)| {
                matches!(standing, Standing::Granted { renew_by_unix_ms, .. } if overdue(*renew_by_unix_ms))
                    && !standing.may_start()
            })
            .map(|(resource, _)| resource.clone())
            .collect::<Vec<_>>();
        let judged_any = !due.is_empty();
        for resource in due {
            let name = resource.clone();
            let judged = self
                .store_op(services, events_rx, &resource, move |store| {
                    store.admit(&name, Request::default())
                })
                .await;
            match judged {
                Ok(renew_by) => {
                    self.claims.insert(resource, Standing::granted(renew_by));
                }
                // Releasing already, see `store_op`.
                Err(_) if self.claims.get(&resource) == Some(&Standing::Releasing) => {}
                Err(CommandRejection::ReservationStore(err)) => {
                    tracing::warn!(%err, resource, "cannot check the reservation; retrying");
                    if let Some(Standing::Granted { judged_until, .. }) =
                        self.claims.get_mut(&resource)
                    {
                        *judged_until = tokio::time::Instant::now() + POLL_INTERVAL;
                    }
                }
                Err(err) => {
                    let detail = format!("service disabled: {}", err.message());
                    self.begin_release(services, &resource, &detail);
                }
            }
        }
        judged_any
    }

    /// Whether a queued request or a release needs the poll timer.
    pub(super) fn needs_poll(&self) -> bool {
        self.claims
            .values()
            .any(|standing| matches!(standing, Standing::Waiting | Standing::Releasing))
    }

    /// Finishes releases whose runs are gone and notices queued requests that were granted or
    /// dropped.
    pub(super) async fn poll_reservations(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
    ) {
        self.settle_releases(services, events_rx).await;
        let waiting = self
            .claims
            .iter()
            .filter(|(_, standing)| **standing == Standing::Waiting)
            .map(|(resource, _)| resource.clone())
            .collect::<Vec<_>>();
        for resource in waiting {
            let name = resource.clone();
            match self
                .store_op(services, events_rx, &resource, move |store| {
                    store.poll(&name)
                })
                .await
            {
                Ok(reply) => self.track(&resource, &reply),
                Err(err) => tracing::warn!(%err, resource, "cannot poll the reservation"),
            }
        }
    }

    /// Records where this session's claim stands after the store answered `reply`.
    fn track(&mut self, resource: &str, reply: &ReservationReply) {
        let standing = match (reply.state, reply.own_claim()) {
            (ReservationState::Granted, Some(claim)) => Standing::granted(claim.renew_by_unix_ms),
            (ReservationState::Waiting { .. }, _) => Standing::Waiting,
            (ReservationState::Releasing, _) => Standing::Releasing,
            (ReservationState::Released | ReservationState::Granted, _) => {
                self.claims.remove(resource);
                return;
            }
        };
        self.claims.insert(resource.to_string(), standing);
    }

    /// The refusal for a start or renewal of `resource` while this session releases it.
    fn releasing_refusal(&self, resource: &str) -> Option<CommandRejection> {
        (self.claims.get(resource) == Some(&Standing::Releasing)).then(|| {
            rejection(&ReservationError::Releasing {
                resource: resource.to_string(),
            })
        })
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
            return Err(self.refuse_foreign(caller, "reserve, renew, or release resources for it"));
        }
        let (resource, reply) = match request {
            ReservationRequest::Reserve {
                resource,
                estimate_secs,
                reason,
            } => {
                if let Some(refusal) = self.releasing_refusal(&resource) {
                    return Err(refusal);
                }
                let request = Request {
                    estimate_secs,
                    reason,
                };
                let name = resource.clone();
                let reply = self
                    .store_op(services, events_rx, &resource, move |store| {
                        store.reserve(&name, request)
                    })
                    .await?;
                self.track(&resource, &reply);
                (resource, reply)
            }
            ReservationRequest::Renew { resource, claim } => {
                if let Some(refusal) = self.releasing_refusal(&resource) {
                    return Err(refusal);
                }
                let name = resource.clone();
                let reply = self
                    .store_op(services, events_rx, &resource, move |store| {
                        store.renew(&name, claim.as_deref())
                    })
                    .await?;
                self.track(&resource, &reply);
                (resource, reply)
            }
            ReservationRequest::Release { resource } => {
                let reply = self.release(services, events_rx, &resource).await?;
                (resource, reply)
            }
        };
        Ok(self.with_local_standing(&resource, reply))
    }

    /// `reply` with the state this session knows locally: the store shows a holder that
    /// releases as the ordinary owner until its runs are gone.
    fn with_local_standing(&self, resource: &str, mut reply: ReservationReply) -> ReservationReply {
        if self.claims.get(resource) == Some(&Standing::Releasing) {
            reply.state = ReservationState::Releasing;
        }
        reply
    }

    /// Releases this session's claim of `resource`: a queued request goes at once, a grant once
    /// the services that need it stopped.
    async fn release(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
        resource: &str,
    ) -> Result<ReservationReply, CommandRejection> {
        match self.claims.get(resource) {
            Some(Standing::Granted { .. }) => {
                let detail =
                    format!("service disabled because this session released resource `{resource}`");
                self.begin_release(services, resource, &detail);
                self.settle_releases(services, events_rx).await;
            }
            Some(Standing::Releasing) => {}
            Some(Standing::Waiting) | None => {
                let name = resource.to_string();
                self.store_op(services, events_rx, resource, move |store| {
                    store.remove(&name)
                })
                .await?;
                self.claims.remove(resource);
            }
        }
        let name = resource.to_string();
        self.store_op(services, events_rx, resource, move |store| {
            store.poll(&name)
        })
        .await
    }

    /// Stops the session for `caller`, unless another project asks while the session holds,
    /// waits for, or releases a resource, or a run of a service that needs one has not ended.
    ///
    /// An accepted stop cancels the session in this same scheduler turn, so nothing is admitted
    /// or started after it.
    pub(super) fn shutdown_for(&self, caller: &Caller) -> Result<(), CommandRejection> {
        if !self.is_own(caller) {
            let running = self
                .services
                .values()
                .filter_map(|runtime| runtime.running.as_ref()?.resource.clone());
            let resources = self
                .claims
                .keys()
                .cloned()
                .chain(running)
                .collect::<BTreeSet<_>>();
            if !resources.is_empty() {
                let resources = resources.into_iter().collect::<Vec<_>>().join(", ");
                return Err(self.refuse_foreign(
                    caller,
                    &format!("stop it while it uses resources {resources}"),
                ));
            }
        }
        self.shutdown.cancel();
        Ok(())
    }

    /// The resources this session holds, waits for, runs, or names in a live service, for
    /// display.
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
        self.claims.keys().cloned().chain(named).collect()
    }

    /// Starts every service that is ready, taking the resource of each that needs one when it
    /// becomes ready.
    ///
    /// A service refused its resource is disabled with one event naming why.
    pub(super) async fn start_ready(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
    ) {
        while !self.shutdown.is_cancelled() {
            let needs_grant =
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
                    claims: &self.claims,
                });
            let Some((service_id, resource)) = needs_grant else {
                break;
            };
            let reason = format!("taken to start {service_id}");
            if let Err(rejection) = self.admit(services, events_rx, &resource, reason).await
                && self
                    .services
                    .get(&service_id)
                    .is_some_and(|runtime| runtime.desired != super::DesiredState::Disabled)
            {
                let detail = format!("service disabled: {}", rejection.message());
                self.disable_for_resource(services, &service_id, &detail);
            }
        }
        self.session_status
            .set_watched_resources(self.watched_resources(services));
    }

    /// Removes this session's claims from the store once its services stopped, as it stops, so
    /// it no longer shows as holder or waiter.
    pub(super) async fn remove_claims_on_stop(&mut self) {
        let Some(store) = self.session.resources.clone() else {
            return;
        };
        if self.claims.is_empty() || self.running_count() > 0 {
            return;
        }
        let resources = std::mem::take(&mut self.claims)
            .into_keys()
            .collect::<Vec<_>>();
        let removed = tokio::task::spawn_blocking(move || {
            for resource in resources {
                if let Err(err) = store.remove(&resource) {
                    tracing::warn!(%err, resource, "cannot remove the reservation on stop");
                }
            }
        })
        .await;
        if let Err(err) = removed {
            tracing::warn!(%err, "cannot remove the reservations on stop");
        }
    }
}
