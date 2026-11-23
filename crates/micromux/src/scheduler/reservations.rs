//! The scheduler's side of resource reservations: which resources this session holds, waits
//! for, or hands over, who may control the services that need them, and the store transactions
//! behind each change.
//!
//! The store records who holds a resource across sessions and its `renew_by`, the one deadline;
//! this side decides when this session starts, stops, and gives up its own runs.
//! The session claims a resource exactly while one of its services needs it: a live run captured
//! it, or a service that names it has a start to come.
//! A named start queues for a resource another session holds, the grant starts what waited for
//! it, and the claim goes once nothing needs it any more.
//! Before the deadline the scheduler only wakes up to look at the wall clock; once the deadline
//! passed it asks the store, which keeps the grant while nobody waits and refuses it once
//! somebody does, so the session hands the resource over.
//! See [ADR 7](https://github.com/romnn/micromux/blob/main/adr/0007-reservations-are-requests-to-run.md)
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

/// How often the scheduler checks on a queued request and on a hand-over in progress, and judges
/// again a grant past its deadline that nobody waited for, so a waiter that arrives later gets the
/// resource within seconds.
pub(super) const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// The longest the scheduler sleeps before looking at the wall clock again while a grant is not
/// yet due, so a suspended machine that wakes past the deadline notices within a minute.
const WALL_CLOCK_LOOK: Duration = Duration::from_mins(1);

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
    /// The session hands the resource over: it stops its services that need it, then removes
    /// its claim.
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
            judged_until: tokio::time::Instant::now() + POLL_INTERVAL,
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
                "this connection did not name its project; run micromux from inside {project} (a \
                 client outside any project names none)"
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
    /// A stopping session still waits for the transaction, which the registry lock's timeout
    /// bounds, so a claim it writes is one the session knows and removes as it stops.
    /// When the store refuses the session's grant because it is past its deadline while others
    /// wait, the session starts handing the resource over.
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
            .await_handling_events(services, events_rx, task)
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
    /// A grant before its check needs no store transaction. A claim this session hands over
    /// stays handed over, and a queued request is refused, since this takes only a free resource.
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
        self.grant(services, resource, renew_by);
        Ok(())
    }

    /// Records that this session holds `resource` until `renew_by_unix_ms`, and tells each
    /// service whose start waited for the session's queued request that it got the resource.
    fn grant(&mut self, services: &ServiceMap, resource: &str, renew_by_unix_ms: u64) {
        let queued = self
            .claims
            .insert(resource.to_string(), Standing::granted(renew_by_unix_ms))
            == Some(Standing::Waiting);
        if !queued {
            return;
        }
        for service_id in self.pending_starts(services, resource) {
            self.append_event(
                &service_id,
                ServiceEventKind::ResourceGranted,
                format!(
                    "resource `{resource}` granted; the service starts once its dependencies are \
                     ready"
                ),
            );
        }
    }

    /// Lets a named start of a service that needs `resource` go ahead, granted or queued: a grant
    /// this session holds is used as it is, and otherwise the store keeps the request the session
    /// has queued, or takes the free resource or queues for it, with `reason` saying why.
    ///
    /// A queued request is confirmed in the store, since it may have lapsed while the machine
    /// slept.
    /// The caller commits the start in the same scheduler turn, so the claim is never without the
    /// start it was made for.
    /// A claim this session hands over stays handed over.
    pub(super) async fn queue_for(
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
        let reply = self
            .store_op(services, events_rx, resource, move |store| {
                store.join(&name, request)
            })
            .await?;
        self.track(services, resource, &reply);
        Ok(())
    }

    /// Whether `service` has a start to come that waits for this session's queued request for
    /// its resource.
    pub(super) fn start_is_queued(
        &self,
        service: &crate::service::Service,
        runtime: &super::ServiceRuntime,
    ) -> bool {
        service
            .spec
            .resource
            .as_ref()
            .is_some_and(|resource| self.claims.get(resource) == Some(&Standing::Waiting))
            && super::schedule::has_pending_start(service, runtime)
    }

    /// `detail` of a start request's event, which says so when the start waits for this
    /// session's queued request for `resource`.
    pub(super) fn request_detail(&self, detail: &str, resource: Option<&str>) -> String {
        match resource {
            Some(resource) if self.claims.get(resource) == Some(&Standing::Waiting) => format!(
                "{detail}; it waits for resource `{resource}`, which another session holds, and \
                 starts once this session gets it"
            ),
            _ => detail.to_string(),
        }
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

    /// The services whose definition names `resource` and that have a start to come.
    fn pending_starts(&self, services: &ServiceMap, resource: &str) -> Vec<ServiceID> {
        services
            .iter()
            .filter(|(service_id, service)| {
                service.spec.resource.as_deref() == Some(resource)
                    && self
                        .services
                        .get(*service_id)
                        .is_some_and(|runtime| super::schedule::has_pending_start(service, runtime))
            })
            .map(|(service_id, _)| service_id.clone())
            .collect()
    }

    /// Whether this session needs `resource`: a live run captured it, retired services included,
    /// or a service that names it has a start to come.
    fn needs_resource(&self, services: &ServiceMap, resource: &str) -> bool {
        self.runs_resource(resource) || !self.pending_starts(services, resource).is_empty()
    }

    /// Disables every service with a start to come that needs `resource`, with `detail` as the
    /// event.
    fn disable_pending_starts(&mut self, services: &ServiceMap, resource: &str, detail: &str) {
        for service_id in self.pending_starts(services, resource) {
            self.disable_for_resource(services, &service_id, detail);
        }
    }

    /// Starts handing `resource` over: refuses further starts and renewals, and disables every
    /// service that needs it, with `detail` as the event, so its runs stop and no start is left.
    ///
    /// A job whose run finished and a manual job that waits to be run need nothing, so they keep
    /// their outcome for the services that depend on them.
    /// The claim stays in the store until the runs are gone, see [`Self::give_up_claims`].
    fn begin_release(&mut self, services: &ServiceMap, resource: &str, detail: &str) {
        self.claims
            .insert(resource.to_string(), Standing::Releasing);
        let running = self
            .services
            .iter()
            .filter(|(_, runtime)| {
                runtime
                    .running
                    .as_ref()
                    .is_some_and(|running| running.resource.as_deref() == Some(resource))
            })
            .map(|(service_id, _)| service_id.clone());
        let affected = running
            .chain(self.pending_starts(services, resource))
            .collect::<BTreeSet<_>>();
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

    /// Removes every claim nothing of this session needs any more: a request or grant no service
    /// needs, and a hand-over whose runs are gone, so that the next request gets the resource.
    ///
    /// Returns whether it removed any claim.
    pub(super) async fn give_up_claims(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
    ) -> bool {
        let done = self
            .claims
            .iter()
            .filter(|(resource, standing)| match standing {
                Standing::Releasing => !self.runs_resource(resource),
                Standing::Waiting | Standing::Granted { .. } => {
                    !self.needs_resource(services, resource)
                }
            })
            .map(|(resource, _)| resource.clone())
            .collect::<Vec<_>>();
        let mut removed_any = false;
        for resource in done {
            if self.shutdown.is_cancelled() {
                break;
            }
            removed_any |= self.give_up(services, events_rx, &resource).await;
        }
        removed_any
    }

    /// Removes this session's claim of `resource` from the store, and returns whether it did.
    ///
    /// A removal that fails leaves the claim being handed over, which the poll retries.
    async fn give_up(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
        resource: &str,
    ) -> bool {
        let name = resource.to_string();
        match self
            .store_op(services, events_rx, resource, move |store| {
                store.remove(&name)
            })
            .await
        {
            Ok(()) => {
                self.claims.remove(resource);
                true
            }
            Err(err) => {
                tracing::warn!(%err, resource, "cannot give up the reservation; retrying");
                self.claims
                    .insert(resource.to_string(), Standing::Releasing);
                false
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
    /// hands over. A grant before its deadline needs no store transaction.
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
            if self.shutdown.is_cancelled() {
                break;
            }
            let name = resource.clone();
            let judged = self
                .store_op(services, events_rx, &resource, move |store| {
                    store.admit(&name, Request::default())
                })
                .await;
            match judged {
                Ok(renew_by) => self.grant(services, &resource, renew_by),
                // Handing over already, see `store_op`.
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

    /// Whether a queued request or a hand-over needs the poll timer.
    pub(super) fn needs_poll(&self) -> bool {
        self.claims
            .values()
            .any(|standing| matches!(standing, Standing::Waiting | Standing::Releasing))
    }

    /// Gives up claims nothing needs any more, hand-overs whose runs are gone included, keeps
    /// queued requests alive, queuing one that lapsed anyway again, and notices those that were
    /// granted.
    pub(super) async fn poll_reservations(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
    ) {
        self.give_up_claims(services, events_rx).await;
        let waiting = self
            .claims
            .iter()
            .filter(|(_, standing)| **standing == Standing::Waiting)
            .map(|(resource, _)| resource.clone())
            .collect::<Vec<_>>();
        for resource in waiting {
            if self.shutdown.is_cancelled() {
                break;
            }
            let name = resource.clone();
            match self
                .store_op(services, events_rx, &resource, move |store| {
                    store.keep_queued(&name)
                })
                .await
            {
                Ok(reply) => self.track(services, &resource, &reply),
                Err(err) => tracing::warn!(%err, resource, "cannot poll the reservation"),
            }
        }
    }

    /// Records where this session's claim stands after the store answered `reply`; a queued
    /// request the store granted tells the services that wait for it.
    fn track(&mut self, services: &ServiceMap, resource: &str, reply: &ReservationReply) {
        match (reply.state, reply.own_claim()) {
            (ReservationState::Granted, Some(claim)) => {
                self.grant(services, resource, claim.renew_by_unix_ms);
            }
            (ReservationState::Waiting { .. }, _) => {
                self.claims.insert(resource.to_string(), Standing::Waiting);
            }
            (ReservationState::Releasing, _) => {
                self.claims
                    .insert(resource.to_string(), Standing::Releasing);
            }
            (ReservationState::Released | ReservationState::Granted, _) => {
                self.claims.remove(resource);
            }
        }
    }

    /// The refusal for a start or renewal of `resource` while this session hands it over.
    fn releasing_refusal(&self, resource: &str) -> Option<CommandRejection> {
        (self.claims.get(resource) == Some(&Standing::Releasing)).then(|| {
            rejection(&ReservationError::Releasing {
                resource: resource.to_string(),
            })
        })
    }

    /// Applies `request` for `caller`, which must be the session's own process or project, except
    /// for a status, which changes nothing.
    pub(super) async fn reservation(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
        caller: &Caller,
        request: ReservationRequest,
    ) -> Result<ReservationReply, CommandRejection> {
        if !self.is_own(caller) && !matches!(request, ReservationRequest::Status { .. }) {
            return Err(self.refuse_foreign(caller, "reserve, renew, or release resources for it"));
        }
        let (resource, reply) = match request {
            ReservationRequest::Reserve {
                resource,
                estimate_secs,
                reason,
            } => {
                let request = Request {
                    estimate_secs,
                    reason,
                };
                let reply = self
                    .reserve(services, events_rx, &resource, request)
                    .await?;
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
                self.track(services, &resource, &reply);
                (resource, reply)
            }
            ReservationRequest::Release { resource } => {
                let reply = self.release(services, events_rx, &resource).await?;
                (resource, reply)
            }
            ReservationRequest::Status { resource } => {
                let reply = self.status(services, events_rx, &resource).await?;
                (resource, reply)
            }
        };
        Ok(self.with_local_standing(&resource, reply))
    }

    /// Reserves `resource` with `request`.
    ///
    /// A request this session has already is renewed and takes the estimate and reason, and
    /// enables nothing, so a service disabled during the turn stays disabled.
    /// A fresh one requests a start of every configured service that names the resource and starts
    /// with the session, see [`Self::reservation_group`], and the session runs them once it gets
    /// the resource.
    async fn reserve(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
        resource: &str,
        request: Request,
    ) -> Result<ReservationReply, CommandRejection> {
        if let Some(refusal) = self.releasing_refusal(resource) {
            return Err(refusal);
        }
        let group = if self.claims.contains_key(resource) {
            Vec::new()
        } else {
            self.reservation_group(services, resource)?
        };
        let name = resource.to_string();
        let reply = self
            .store_op(services, events_rx, resource, move |store| {
                store.reserve(&name, request)
            })
            .await?;
        // The starts are committed in the same scheduler turn as the claim they need.
        for service_id in &group {
            let Some(runtime) = self.services.get_mut(service_id) else {
                continue;
            };
            if services
                .get(service_id)
                .is_some_and(|service| service.spec.kind == crate::ServiceKind::Job)
            {
                if runtime.desired == super::DesiredState::Disabled {
                    runtime.enable_without_start();
                }
            } else {
                runtime.request_enable();
            }
            self.sync(services, service_id);
            self.append_event(
                service_id,
                ServiceEventKind::EnableRequested,
                format!("service enable requested by the reservation of resource `{resource}`"),
            );
        }
        self.track(services, resource, &reply);
        Ok(reply)
    }

    /// The services a fresh reservation of `resource` starts: the live configured services that
    /// name it and start with the session, so neither one with `disabled: true` nor a manual job.
    ///
    /// A job among them keeps the outcome of a run that finished on its own.
    /// A dynamic service is never part of it, though a start of one that is to come runs once the
    /// session gets the resource.
    /// Refuses a reservation that would start nothing, which would only keep others waiting.
    pub(super) fn reservation_group(
        &self,
        services: &ServiceMap,
        resource: &str,
    ) -> Result<Vec<ServiceID>, CommandRejection> {
        let mut group = Vec::new();
        let mut starts_any = false;
        for (service_id, service) in services {
            let Some(runtime) = self.services.get(service_id) else {
                continue;
            };
            if service.spec.resource.as_deref() != Some(resource)
                || service.origin != crate::ServiceOrigin::Configured
                || service.startup_mode != crate::service::StartupMode::Enabled
                || !runtime.is_live()
            {
                continue;
            }
            starts_any |= match (service.spec.kind, runtime.desired) {
                // Enabling a service requests its start.
                (crate::ServiceKind::Service, _) => true,
                (crate::ServiceKind::Job, super::DesiredState::Enabled) => {
                    super::schedule::has_pending_start(service, runtime)
                }
                // Enabling a disabled job returns it to its outcome, which its restart policy may
                // retry, or to pending.
                (crate::ServiceKind::Job, super::DesiredState::Disabled) => {
                    match runtime.resumed_job_state() {
                        super::State::Exited { exit_code } => {
                            runtime.will_auto_restart(&service.spec.restart, exit_code)
                        }
                        _ => true,
                    }
                }
            };
            group.push(service_id.clone());
        }
        if !starts_any {
            return Err(CommandRejection::InvalidState(format!(
                "no service of this session that starts with it needs resource `{resource}`, so a \
                 reservation would start nothing; enable the service that needs it instead"
            )));
        }
        Ok(group)
    }

    /// `reply` with the state this session knows locally: the store shows a holder that hands
    /// the resource over as the ordinary owner until its runs are gone.
    fn with_local_standing(&self, resource: &str, mut reply: ReservationReply) -> ReservationReply {
        if self.claims.get(resource) == Some(&Standing::Releasing) {
            reply.state = ReservationState::Releasing;
        }
        reply
    }

    /// Where this session's request for `resource` stands in the file as last written, which
    /// changes nothing: it neither renews a request nor extends a grant.
    async fn status(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
        resource: &str,
    ) -> Result<ReservationReply, CommandRejection> {
        let name = resource.to_string();
        self.store_op(services, events_rx, resource, move |store| {
            store.status(&name)
        })
        .await
    }

    /// Releases this session's claim of `resource`.
    ///
    /// A queued request goes at once, with the starts that waited for it.
    /// A grant becomes due now: it passes on as soon as another request waits, which may be at
    /// once, and until then the services keep running.
    async fn release(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
        resource: &str,
    ) -> Result<ReservationReply, CommandRejection> {
        match self.claims.get(resource) {
            Some(Standing::Waiting) => {
                let detail = format!(
                    "service disabled because this session released its queued request for \
                     resource `{resource}`"
                );
                self.disable_pending_starts(services, resource, &detail);
                self.give_up(services, events_rx, resource).await;
            }
            Some(Standing::Granted { .. }) => {
                let name = resource.to_string();
                let reply = self
                    .store_op(services, events_rx, resource, move |store| {
                        store.yield_turn(&name)
                    })
                    .await?;
                self.track(services, resource, &reply);
                let waiting = reply.view.waiters.len();
                if waiting > 0
                    && matches!(self.claims.get(resource), Some(Standing::Granted { .. }))
                {
                    let detail = format!(
                        "service disabled: this session released resource `{resource}` while \
                         {waiting} request(s) wait"
                    );
                    self.begin_release(services, resource, &detail);
                    if !self.runs_resource(resource) {
                        self.give_up(services, events_rx, resource).await;
                    }
                }
            }
            Some(Standing::Releasing) | None => {}
        }
        self.status(services, events_rx, resource).await
    }

    /// Stops the session for `caller`, unless another project asks while the session holds,
    /// waits for, or hands over a resource, or a run of a service that needs one has not ended.
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

    /// Starts every service that is ready, then takes the resource of the first ready service
    /// that needs one, or gives up the claims nothing needs any more, and returns whether that
    /// changed anything, after which another pass is due.
    ///
    /// Every service is visited before that store transaction, so a reply sent after the pass sees
    /// each one's dependencies as they are.
    ///
    /// A service refused its resource is disabled with one event naming why.
    /// It ends at that store transaction, so the scheduler applies the commands that arrived
    /// meanwhile before the next pass starts anything.
    /// Claims are given up only here, after a command committed the starts it requested, never
    /// while a command awaits the store.
    pub(super) async fn start_ready(
        &mut self,
        services: &ServiceMap,
        events_rx: &mut mpsc::Receiver<ProcessEvent>,
    ) -> bool {
        if self.shutdown.is_cancelled() {
            return false;
        }
        let needs_grant = super::schedule::schedule_ready(&mut super::schedule::ScheduleContext {
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
        let changed = match needs_grant {
            Some((service_id, resource)) => {
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
                true
            }
            None => self.give_up_claims(services, events_rx).await,
        };
        self.session_status
            .set_watched_resources(self.watched_resources(services));
        changed
    }

    /// Removes this session's queued requests and the grants no live run needs, as the session
    /// starts to stop, so that no other session hands a resource over to it.
    ///
    /// A grant a live run needs stays until the run is gone, see [`Self::remove_claims_on_stop`].
    pub(super) async fn remove_idle_claims_on_stop(&mut self) {
        let Some(store) = self.session.resources.clone() else {
            return;
        };
        let idle = self
            .claims
            .iter()
            .filter(|(resource, standing)| {
                **standing == Standing::Waiting || !self.runs_resource(resource)
            })
            .map(|(resource, _)| resource.clone())
            .collect::<Vec<_>>();
        if idle.is_empty() {
            return;
        }
        for resource in &idle {
            self.claims.remove(resource);
        }
        Self::remove_on_stop(store, idle).await;
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
        Self::remove_on_stop(store, resources).await;
    }

    async fn remove_on_stop(store: ResourceStore, resources: Vec<String>) {
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
