//! Reservations of shared resources, each of which one session at a time may run.
//!
//! A service that names a resource runs only in the session that holds the resource.
//! Each resource has two files in the `resources` directory of the port registry:
//!
//! - `<resource>.json` records the owner and the queue of waiting requests, first in first out.
//! - `<resource>.fence` is locked shared by the shims of the resource's services, see below.
//!
//! Every change is one transaction under the registry lock.
//! It first drops the claims of dead session instances and expired waiters, then grants the
//! resource to the head of the queue once it has no owner and its fence is free.
//!
//! The holder's `renew_by` is the one deadline, kept in wall-clock time, and only the holder
//! judges it, in the transaction that admits, reserves, or renews for it: a holder past its
//! deadline goes on while nobody waits, and is refused, so that it hands the resource over, once
//! somebody does.
//! A release makes the deadline due at once, so the holder keeps running while nobody waits.
//! A holder hands over by stopping its services that name the resource, then removing its claim;
//! until then the file shows it as the ordinary owner.
//!
//! # Handoff
//!
//! A waiter is promoted only when nothing holds the fence at that instant.
//! The supervisor locks the fence shared before it starts a run of a service that names the
//! resource and keeps it until the run exited; the run's shim locks it too, through its teardown
//! after its supervisor died, and the supervisor hands its locked descriptor to the cleanup-only
//! shim of the run's cleanup, which keeps it until the cleanup ended.
//! So the next holder starts only once the previous holder's processes and cleanups are gone,
//! even when the previous holder died without stopping them.
//! Reservations need containment, so a config that names a resource is refused where micromux
//! has none.
//!
//! The decisions behind this design are recorded in
//! [ADR 7](https://github.com/romnn/micromux/blob/main/adr/0007-reservations-are-requests-to-run.md).
//!
//! A session instance is live while it holds its instance lock and its port record still names
//! the instance's token, so a restart of the same config does not inherit the old instance's
//! claims.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::registry::{REGISTRY_LOCK, lock_registry, owner_is_live, read_record};
use super::{PortRegistry, SessionError, create_private_dir, is_hostname_label};

/// How long a grant or a queued request holds without being renewed.
const RENEW_INTERVAL: Duration = Duration::from_hours(1);

/// Longest reason a request may give.
const MAX_REASON_CHARS: usize = 200;

/// Most requests that may queue for one resource.
const MAX_WAITERS: usize = 16;

/// Smallest move of a waiter's deadline that a renewal writes, so a polling `wait` does not
/// rewrite the file every few seconds.
const MIN_WAITER_RENEWAL_MS: u64 = 60_000;

/// Subdirectory of the registry holding the reservation files, apart from the port records.
const RESOURCES_DIRECTORY: &str = "resources";

/// One resource's holder and queue.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResourceView {
    /// The resource's name.
    pub name: String,
    /// The session holding the resource, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Owner>,
    /// Requests waiting for the resource, first in first out.
    #[serde(default)]
    pub waiters: Vec<Claim>,
}

/// The session holding a resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Owner {
    /// The holder's request.
    pub claim: Claim,
    /// When the resource was granted, in Unix milliseconds.
    pub granted_at_unix_ms: u64,
}

/// One session's request for a resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Claim {
    /// Identifies this request; a new request after a release gets a new id.
    pub id: String,
    /// Id of the requesting session: the digest of its canonical config path.
    pub session_id: String,
    /// Token of the requesting session instance.
    pub instance: String,
    /// Name of the requesting session.
    pub session_name: String,
    /// Canonical config path of the requesting session.
    pub config_path: String,
    /// How long the requester expects to hold the resource, for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimate_secs: Option<u64>,
    /// Why the requester wants the resource, for display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// When the request was made, in Unix milliseconds.
    pub requested_at_unix_ms: u64,
    /// When the grant or the queued request lapses unless renewed, in Unix milliseconds.
    pub renew_by_unix_ms: u64,
}

/// Where a session's request for a resource stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReservationState {
    /// The session holds the resource.
    Granted,
    /// The request waits in the queue.
    Waiting {
        /// One-based position in the queue.
        position: usize,
    },
    /// The session stops its services that name the resource, and then releases it.
    Releasing,
    /// The session neither holds nor waits for the resource.
    Released,
}

/// A resource's holder and queue together with where one session's request stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReservationReply {
    /// The resource's holder and queue.
    pub view: ResourceView,
    /// The id of the session's own request in `view`, absent once released.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<String>,
    /// Where the session's request stands.
    pub state: ReservationState,
}

impl ReservationReply {
    /// The session's own request in [`Self::view`], as owner or waiter.
    #[must_use]
    pub fn own_claim(&self) -> Option<&Claim> {
        let id = self.claim.as_deref()?;
        self.view
            .owner
            .iter()
            .map(|owner| &owner.claim)
            .chain(&self.view.waiters)
            .find(|claim| claim.id == id)
    }
}

/// A request for display: its session and worktree, and the estimate and reason it gave.
impl std::fmt::Display for Claim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let worktree = Path::new(&self.config_path)
            .parent()
            .map_or_else(|| self.config_path.clone(), |dir| dir.display().to_string());
        write!(f, "{} ({worktree})", self.session_name)?;
        if let Some(estimate) = self.estimate_secs {
            write!(
                f,
                ", estimated {}",
                humantime::format_duration(Duration::from_secs(estimate))
            )?;
        }
        if let Some(reason) = &self.reason {
            write!(f, ", reason {reason:?}")?;
        }
        Ok(())
    }
}

/// Why a reservation operation failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReservationError {
    /// The registry could not be located or locked.
    #[error(transparent)]
    Registry(#[from] SessionError),
    /// A reservation file could not be accessed.
    #[error("failed to access the reservation file {}", path.display())]
    Io {
        /// Path of the file.
        path: PathBuf,
        /// Underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
    /// A reservation file exists but cannot be parsed, so nobody can tell who holds the resource.
    #[error("cannot read the reservation file {}: {message}", path.display())]
    Unreadable {
        /// Path of the file.
        path: PathBuf,
        /// What went wrong.
        message: String,
    },
    /// The resource name is not a lowercase hostname label.
    #[error(
        "resource `{resource}` must be a lowercase name (a-z, 0-9, inner hyphens, at most 63 \
         characters)"
    )]
    InvalidName {
        /// The rejected name.
        resource: String,
    },
    /// Another session holds the resource.
    #[error(
        "resource `{resource}` is held: queue with `micromux ctl reserve {resource}` (MCP: \
         reserve_resource) or by enabling the service, which then starts once granted; held by \
         session {}, {}",
        owner.claim,
        crate::renew_label(owner.claim.renew_by_unix_ms)
    )]
    Held {
        /// The resource.
        resource: String,
        /// Its holder.
        owner: Box<Owner>,
    },
    /// Other sessions queue for the resource.
    #[error(
        "resource `{resource}` is queued for: queue with `micromux ctl reserve {resource}` (MCP: \
         reserve_resource) or by enabling the service, which then starts once granted; {waiting} \
         request(s) already wait"
    )]
    Queued {
        /// The resource.
        resource: String,
        /// How many requests wait.
        waiting: usize,
    },
    /// This session's request waits in the queue.
    #[error(
        "resource `{resource}` is queued for: wait with `micromux ctl wait --reservation \
         {resource}` (MCP: wait_for_reservation); this session already waits at position \
         {position}"
    )]
    Waiting {
        /// The resource.
        resource: String,
        /// One-based position in the queue.
        position: usize,
    },
    /// Nobody holds the resource, but processes of its previous holder still run.
    #[error(
        "resource `{resource}` is free, but services of its previous holder are still stopping; \
         try again shortly"
    )]
    Stopping {
        /// The resource.
        resource: String,
    },
    /// This session hands the resource over to a waiting request.
    #[error(
        "resource `{resource}` is being handed over to a waiting session: queue for it again with \
         `micromux ctl reserve {resource}` (MCP: reserve_resource) once this session's services \
         that need it stopped"
    )]
    Releasing {
        /// The resource.
        resource: String,
    },
    /// This session's grant is past its `renew_by` deadline while other sessions wait, so the
    /// session releases it.
    #[error(
        "resource `{resource}` is being handed over: queue for it again with `micromux ctl \
         reserve {resource}` (MCP: reserve_resource); this session's grant was due while \
         {waiting} request(s) wait"
    )]
    Lapsed {
        /// The resource.
        resource: String,
        /// How many requests wait.
        waiting: usize,
    },
    /// This session has no request for the resource, or not the one named.
    #[error(
        "this session has no reservation of resource `{resource}`; reserve it first with \
         `micromux ctl reserve {resource}` (MCP: reserve_resource)"
    )]
    Gone {
        /// The resource.
        resource: String,
    },
    /// The queue is full.
    #[error("{MAX_WAITERS} requests already queue for resource `{resource}`")]
    QueueFull {
        /// The resource.
        resource: String,
    },
    /// The reason is too long.
    #[error("a reservation reason may have at most {MAX_REASON_CHARS} characters")]
    ReasonTooLong,
}

/// The session instance a store acts for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Holder {
    /// Digest of the canonical config path, which names the session's registry files.
    pub(crate) session_id: String,
    /// Token of this session instance in its registry record.
    pub(crate) instance: String,
    /// Session name, for display.
    pub(crate) session_name: String,
    /// Canonical config path, for display.
    pub(crate) config_path: String,
}

impl Holder {
    fn holds(&self, claim: &Claim) -> bool {
        claim.session_id == self.session_id && claim.instance == self.instance
    }

    fn claim(&self, id: String, request: Request, now: u64) -> Claim {
        Claim {
            id,
            session_id: self.session_id.clone(),
            instance: self.instance.clone(),
            session_name: self.session_name.clone(),
            config_path: self.config_path.clone(),
            estimate_secs: request.estimate_secs,
            reason: request.reason,
            requested_at_unix_ms: now,
            renew_by_unix_ms: renew_by(now),
        }
    }
}

/// A resource's fence, locked shared for one run.
#[derive(Debug)]
pub(crate) struct Fence {
    /// The fence file, which the run's shim locks by itself as well.
    pub(crate) path: PathBuf,
    /// The locked file. The lock lasts as long as this file or a descriptor duplicated from it
    /// is open, such as the one a cleanup-only shim inherits.
    pub(crate) lock: File,
}

/// What a reservation request says about itself, for display.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) estimate_secs: Option<u64>,
    pub(crate) reason: Option<String>,
}

/// What a request does to a claim its session has already.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Existing {
    /// Renews the claim and takes the estimate and reason the request gives, keeping those it
    /// leaves out.
    Update,
    /// Leaves the claim as it is: its place, deadline, estimate and reason.
    Keep,
}

fn renew_by(now: u64) -> u64 {
    let interval = u64::try_from(RENEW_INTERVAL.as_millis()).unwrap_or(u64::MAX);
    now.saturating_add(interval)
}

/// Moves a waiter's deadline, but only by a minute or more, so that polling does not rewrite the
/// file every few seconds.
fn renew_waiter(claim: &mut Claim, now: u64) {
    let next = renew_by(now);
    if next.saturating_sub(claim.renew_by_unix_ms) >= MIN_WAITER_RENEWAL_MS {
        claim.renew_by_unix_ms = next;
    }
}

/// The content of a reservation file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ResourceFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<Owner>,
    #[serde(default)]
    waiters: Vec<Claim>,
}

impl ResourceFile {
    /// Drops the claims of dead instances and expired waiters, then grants the resource to the
    /// head of the queue when it has no owner and no process of a previous holder is left.
    ///
    /// A grant past its deadline stays: only its holder judges it, see [`Self::judge`].
    fn normalize(&mut self, now: u64, live: impl Fn(&Claim) -> bool, fence_free: bool) {
        if self.owner.as_ref().is_some_and(|owner| !live(&owner.claim)) {
            self.owner = None;
        }
        self.waiters
            .retain(|claim| live(claim) && claim.renew_by_unix_ms > now);
        if self.owner.is_none() && fence_free && !self.waiters.is_empty() {
            let mut claim = self.waiters.remove(0);
            claim.renew_by_unix_ms = renew_by(now);
            self.owner = Some(Owner {
                claim,
                granted_at_unix_ms: now,
            });
        }
    }

    /// Refuses the holder `owner` once it is past its deadline while requests wait, so that it
    /// releases the resource; a holder past its deadline that nobody waits for goes on.
    fn judge(&self, resource: &str, owner: &Owner, now: u64) -> Result<(), ReservationError> {
        if owner.claim.renew_by_unix_ms <= now && !self.waiters.is_empty() {
            return Err(ReservationError::Lapsed {
                resource: resource.to_string(),
                waiting: self.waiters.len(),
            });
        }
        Ok(())
    }

    fn mine_mut<'a>(&'a mut self, me: &Holder) -> Option<&'a mut Owner> {
        self.owner.as_mut().filter(|owner| me.holds(&owner.claim))
    }

    fn waiter_mut<'a>(&'a mut self, me: &Holder) -> Option<&'a mut Claim> {
        self.waiters.iter_mut().find(|claim| me.holds(claim))
    }

    /// Whether `me` may start the resource's services now: `Some(renew_by)` when it holds the
    /// resource, `None` when the resource is free to take.
    fn admission(
        &self,
        resource: &str,
        me: &Holder,
        now: u64,
        fence_free: bool,
    ) -> Result<Option<u64>, ReservationError> {
        if let Some(index) = self.waiters.iter().position(|claim| me.holds(claim)) {
            return Err(ReservationError::Waiting {
                resource: resource.to_string(),
                position: index + 1,
            });
        }
        match &self.owner {
            Some(owner) if me.holds(&owner.claim) => {
                self.judge(resource, owner, now)?;
                Ok(Some(owner.claim.renew_by_unix_ms))
            }
            Some(owner) => Err(ReservationError::Held {
                resource: resource.to_string(),
                owner: Box::new(owner.clone()),
            }),
            None => {
                let resource = resource.to_string();
                if !self.waiters.is_empty() {
                    Err(ReservationError::Queued {
                        resource,
                        waiting: self.waiters.len(),
                    })
                } else if !fence_free {
                    Err(ReservationError::Stopping { resource })
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Takes the resource for `me` when it is free, as a reserve with `request` would, and
    /// returns the grant's `renew_by`.
    fn admit(
        &mut self,
        resource: &str,
        me: &Holder,
        now: u64,
        fence_free: bool,
        request: Request,
        claim_id: impl FnOnce() -> String,
    ) -> Result<u64, ReservationError> {
        if let Some(renew_by) = self.admission(resource, me, now, fence_free)? {
            return Ok(renew_by);
        }
        let claim = me.claim(claim_id(), request, now);
        let renew_by = claim.renew_by_unix_ms;
        self.owner = Some(Owner {
            claim,
            granted_at_unix_ms: now,
        });
        Ok(renew_by)
    }

    /// Queues a request of `me` with `request`, or handles the one it has as `existing` says.
    ///
    /// The transaction's final normalization grants a request queued for a free resource.
    fn enqueue(
        &mut self,
        resource: &str,
        me: &Holder,
        now: u64,
        request: Request,
        existing: Existing,
        claim_id: impl FnOnce() -> String,
    ) -> Result<(), ReservationError> {
        if request
            .reason
            .as_ref()
            .is_some_and(|reason| reason.chars().count() > MAX_REASON_CHARS)
        {
            return Err(ReservationError::ReasonTooLong);
        }
        if let Some(owner) = self.owner.as_ref().filter(|owner| me.holds(&owner.claim)) {
            self.judge(resource, owner, now)?;
        }
        let update = |claim: &mut Claim| {
            if existing == Existing::Keep {
                return;
            }
            if request.estimate_secs.is_some() {
                claim.estimate_secs = request.estimate_secs;
            }
            if request.reason.is_some() {
                claim.reason.clone_from(&request.reason);
            }
            claim.renew_by_unix_ms = renew_by(now);
        };
        if let Some(owner) = self.mine_mut(me) {
            update(&mut owner.claim);
            return Ok(());
        }
        if let Some(claim) = self.waiter_mut(me) {
            update(claim);
            return Ok(());
        }
        if self.waiters.len() >= MAX_WAITERS {
            return Err(ReservationError::QueueFull {
                resource: resource.to_string(),
            });
        }
        self.waiters.push(me.claim(claim_id(), request, now));
        Ok(())
    }

    /// Moves the deadline of the request of `me`, the one with `claim_id` when given.
    fn renew(
        &mut self,
        resource: &str,
        me: &Holder,
        now: u64,
        claim_id: Option<&str>,
    ) -> Result<(), ReservationError> {
        let named = |claim: &Claim| claim_id.is_none_or(|id| id == claim.id);
        if let Some(owner) = self.owner.as_ref().filter(|owner| me.holds(&owner.claim)) {
            if !named(&owner.claim) {
                return Err(ReservationError::Gone {
                    resource: resource.to_string(),
                });
            }
            self.judge(resource, owner, now)?;
        }
        if let Some(owner) = self.mine_mut(me) {
            owner.claim.renew_by_unix_ms = renew_by(now);
            return Ok(());
        }
        let resource = resource.to_string();
        match self.waiter_mut(me) {
            Some(claim) if named(claim) => {
                renew_waiter(claim, now);
                Ok(())
            }
            _ => Err(ReservationError::Gone { resource }),
        }
    }

    /// Makes the grant of `me` due now, unless it is due already, so that it passes on as soon as
    /// a request waits and goes on while none does.
    fn yield_turn(&mut self, me: &Holder, now: u64) {
        if let Some(owner) = self.mine_mut(me) {
            owner.claim.renew_by_unix_ms = owner.claim.renew_by_unix_ms.min(now);
        }
    }

    /// Removes every claim of `me`; the holder removes its grant once none of its processes for
    /// the resource is left.
    fn remove(&mut self, me: &Holder) {
        if self.mine_mut(me).is_some() {
            self.owner = None;
        }
        self.waiters.retain(|claim| !me.holds(claim));
    }

    fn view(&self, resource: &str) -> ResourceView {
        ResourceView {
            name: resource.to_string(),
            owner: self.owner.clone(),
            waiters: self.waiters.clone(),
        }
    }

    fn reply(&self, resource: &str, me: &Holder) -> ReservationReply {
        let owner = self.owner.as_ref().filter(|owner| me.holds(&owner.claim));
        let waiter = self
            .waiters
            .iter()
            .enumerate()
            .find(|(_, claim)| me.holds(claim));
        let (claim, state) = match (owner, waiter) {
            (Some(owner), _) => (Some(owner.claim.id.clone()), ReservationState::Granted),
            (None, Some((index, claim))) => (
                Some(claim.id.clone()),
                ReservationState::Waiting {
                    position: index + 1,
                },
            ),
            (None, None) => (None, ReservationState::Released),
        };
        ReservationReply {
            view: self.view(resource),
            claim,
            state,
        }
    }
}

/// Reads and changes the reservations of one session instance.
///
/// Every operation blocks on the filesystem and the registry lock.
#[derive(Debug, Clone)]
pub(crate) struct ResourceStore {
    /// The port registry directory.
    directory: PathBuf,
    lock_timeout: Duration,
    me: Holder,
    next_claim: Arc<AtomicU64>,
}

impl ResourceStore {
    /// A store for `me` in `registry`.
    pub(crate) fn new(registry: &PortRegistry, me: Holder) -> Result<Self, SessionError> {
        Ok(Self {
            directory: registry.directory()?,
            lock_timeout: registry.lock_timeout,
            me,
            next_claim: Arc::new(AtomicU64::new(0)),
        })
    }

    /// The id of the session the store acts for.
    pub(crate) fn session_id(&self) -> &str {
        &self.me.session_id
    }

    /// The canonical config path of the session the store acts for.
    pub(crate) fn config_path(&self) -> &str {
        &self.me.config_path
    }

    /// The file that every process of a service naming `resource` locks shared while it runs.
    pub(crate) fn fence_path(&self, resource: &str) -> PathBuf {
        self.directory
            .join(RESOURCES_DIRECTORY)
            .join(format!("{resource}.fence"))
    }

    /// Lets this session start the services of `resource`: it holds the resource already, or
    /// takes it while it is free, with `request` saying why.
    ///
    /// Returns the grant's `renew_by` in Unix milliseconds, which is past for a holder past its
    /// deadline that nobody waits for.
    pub(crate) fn admit(&self, resource: &str, request: Request) -> Result<u64, ReservationError> {
        let mut renew_by = 0;
        self.transaction(resource, |file, now, fence_free| {
            renew_by = file.admit(resource, &self.me, now, fence_free, request, || {
                self.claim_id()
            })?;
            Ok(())
        })?;
        Ok(renew_by)
    }

    /// Queues a request for `resource`, granted at once while it is free, or updates and renews
    /// this session's request.
    pub(crate) fn reserve(
        &self,
        resource: &str,
        request: Request,
    ) -> Result<ReservationReply, ReservationError> {
        self.enqueue(resource, request, Existing::Update)
    }

    /// Queues a request for `resource`, granted at once while it is free, unless this session has
    /// one already, which keeps its place and deadline.
    pub(crate) fn join(
        &self,
        resource: &str,
        request: Request,
    ) -> Result<ReservationReply, ReservationError> {
        self.enqueue(resource, request, Existing::Keep)
    }

    fn enqueue(
        &self,
        resource: &str,
        request: Request,
        existing: Existing,
    ) -> Result<ReservationReply, ReservationError> {
        let file = self.transaction(resource, |file, now, _| {
            file.enqueue(resource, &self.me, now, request, existing, || {
                self.claim_id()
            })
        })?;
        Ok(file.reply(resource, &self.me))
    }

    /// Makes this session's grant of `resource` due now, so that it passes on as soon as a
    /// request waits; the reply's queue shows whether one waits already.
    pub(crate) fn yield_turn(&self, resource: &str) -> Result<ReservationReply, ReservationError> {
        let file = self.transaction(resource, |file, now, _| {
            file.yield_turn(&self.me, now);
            Ok(())
        })?;
        Ok(file.reply(resource, &self.me))
    }

    /// Moves the deadline of this session's request for `resource`, the one with `claim_id`
    /// when given.
    pub(crate) fn renew(
        &self,
        resource: &str,
        claim_id: Option<&str>,
    ) -> Result<ReservationReply, ReservationError> {
        let file = self.transaction(resource, |file, now, _| {
            file.renew(resource, &self.me, now, claim_id)
        })?;
        Ok(file.reply(resource, &self.me))
    }

    /// Reports where this session's request for `resource` stands, after granting the resource
    /// to the head of the queue when it can.
    #[cfg(test)]
    pub(crate) fn poll(&self, resource: &str) -> Result<ReservationReply, ReservationError> {
        let file = self.transaction(resource, |_, _, _| Ok(()))?;
        Ok(file.reply(resource, &self.me))
    }

    /// Keeps this session's queued request for `resource` from lapsing, and queues it again at the
    /// end of the queue when it lapsed anyway, as across a long suspend, since the session still
    /// wants the resource; a grant is reported as it is.
    pub(crate) fn keep_queued(&self, resource: &str) -> Result<ReservationReply, ReservationError> {
        let file = self.transaction(resource, |file, now, _| {
            if file.mine_mut(&self.me).is_some() {
                return Ok(());
            }
            if let Some(claim) = file.waiter_mut(&self.me) {
                renew_waiter(claim, now);
                return Ok(());
            }
            let request = Request {
                estimate_secs: None,
                reason: Some("queued again after the request lapsed".to_string()),
            };
            file.enqueue(resource, &self.me, now, request, Existing::Keep, || {
                self.claim_id()
            })
        })?;
        Ok(file.reply(resource, &self.me))
    }

    /// Reports where this session's request for `resource` stands in the file as last written,
    /// without the registry lock and without writing.
    ///
    /// The session normalizes the file on its own poll, so a grant shows here within a poll
    /// interval.
    pub(crate) fn status(&self, resource: &str) -> Result<ReservationReply, ReservationError> {
        let path = self.file_path(resource)?;
        Ok(read_file(&path)?.reply(resource, &self.me))
    }

    /// Removes this session's claims of `resource`, which a holder does once no process of it
    /// for the resource is left; removing what is already gone changes nothing.
    pub(crate) fn remove(&self, resource: &str) -> Result<(), ReservationError> {
        self.transaction(resource, |file, _, _| {
            file.remove(&self.me);
            Ok(())
        })
        .map(|_| ())
    }

    /// Opens the fence of `resource` and locks it shared, for a run of a service that names it.
    pub(crate) fn lock_fence(&self, resource: &str) -> std::io::Result<Fence> {
        let resources = self.directory.join(RESOURCES_DIRECTORY);
        create_private_dir(&resources)?;
        let path = self.fence_path(resource);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        lock.lock_shared()?;
        Ok(Fence { path, lock })
    }

    /// The holder and queue of `resource` as last written, which may still show claims of
    /// sessions that died until a session normalizes the file.
    ///
    /// It neither takes the registry lock nor writes: the file is replaced atomically, so the
    /// read sees one consistent version.
    pub(crate) fn written_view(&self, resource: &str) -> Result<ResourceView, ReservationError> {
        let path = self.file_path(resource)?;
        Ok(read_file(&path)?.view(resource))
    }

    fn claim_id(&self) -> String {
        let counter = self.next_claim.fetch_add(1, Ordering::Relaxed);
        format!("{}-{counter}", self.me.instance)
    }

    fn file_path(&self, resource: &str) -> Result<PathBuf, ReservationError> {
        if !is_hostname_label(resource) {
            return Err(ReservationError::InvalidName {
                resource: resource.to_string(),
            });
        }
        Ok(self
            .directory
            .join(RESOURCES_DIRECTORY)
            .join(format!("{resource}.json")))
    }

    /// Runs `apply` on the normalized file of `resource` under the registry lock and returns the
    /// file as written.
    ///
    /// `apply` gets the current time in Unix milliseconds and whether the fence is free.
    /// The normalization is written even when `apply` fails, and nothing is written when nothing
    /// changed.
    fn transaction(
        &self,
        resource: &str,
        apply: impl FnOnce(&mut ResourceFile, u64, bool) -> Result<(), ReservationError>,
    ) -> Result<ResourceFile, ReservationError> {
        let path = self.file_path(resource)?;
        let _registry_lock = lock_registry(&self.directory.join(REGISTRY_LOCK), self.lock_timeout)?;
        let resources = self.directory.join(RESOURCES_DIRECTORY);
        create_private_dir(&resources).map_err(|source| ReservationError::Io {
            path: resources.clone(),
            source,
        })?;
        let original = read_file(&path)?;
        let fence = self.fence_path(resource);
        let fence_free = fence_is_free(&fence).map_err(|source| ReservationError::Io {
            path: fence,
            source,
        })?;
        let now = unix_now_ms();
        let live = |claim: &Claim| self.me.holds(claim) || self.instance_is_live(claim);

        let mut file = original.clone();
        file.normalize(now, live, fence_free);
        let applied = apply(&mut file, now, fence_free);
        if applied.is_ok() {
            file.normalize(now, live, fence_free);
        }
        if file != original {
            write_file(&resources, &path, &file)
                .map_err(|source| ReservationError::Io { path, source })?;
        }
        applied.map(|()| file)
    }

    /// Whether the session instance that made `claim` still runs.
    ///
    /// A probe that fails counts as live, so an error never frees a resource.
    fn instance_is_live(&self, claim: &Claim) -> bool {
        let lock = self.directory.join(format!("{}.lock", claim.session_id));
        if matches!(owner_is_live(&lock), Ok(false)) {
            return false;
        }
        let record = self.directory.join(format!("{}.json", claim.session_id));
        match read_record(&record) {
            Ok(Some(record)) => record.instance.as_deref() == Some(claim.instance.as_str()),
            Ok(None) => false,
            Err(_) => true,
        }
    }
}

fn read_file(path: &Path) -> Result<ResourceFile, ReservationError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|err| ReservationError::Unreadable {
            path: path.to_path_buf(),
            message: err.to_string(),
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(ResourceFile::default()),
        Err(source) => Err(ReservationError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Replaces `path` atomically, so an unlocked reader sees the old or the new file.
///
/// It is not synced: a crash that loses the change at worst loses a claim, which its session
/// notices on its next renewal.
fn write_file(directory: &Path, path: &Path, file: &ResourceFile) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(file).map_err(std::io::Error::other)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(&bytes)?;
    temporary.persist(path).map_err(|err| err.error)?;
    Ok(())
}

/// Whether no process holds the fence at `path`, probed with an exclusive lock released at once.
fn fence_is_free(path: &Path) -> std::io::Result<bool> {
    let file: File = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => {
            file.unlock()?;
            Ok(true)
        }
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(err)) => Err(err),
    }
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;
    use similar_asserts::assert_eq;

    const NOW: u64 = 1_000_000;

    fn holder(name: &str) -> Holder {
        Holder {
            session_id: format!("{name}-id"),
            instance: format!("{name}-instance"),
            session_name: name.to_string(),
            config_path: format!("/projects/{name}/micromux.yaml"),
        }
    }

    fn everyone_lives(_: &Claim) -> bool {
        true
    }

    fn queued(file: &mut ResourceFile, me: &Holder, now: u64) -> eyre::Result<()> {
        file.enqueue(
            "desk",
            me,
            now,
            Request::default(),
            Existing::Update,
            || format!("{}-claim", me.session_name),
        )?;
        Ok(())
    }

    fn owner_name(file: &ResourceFile) -> Option<&str> {
        file.owner
            .as_ref()
            .map(|owner| owner.claim.session_name.as_str())
    }

    fn waiter_names(file: &ResourceFile) -> Vec<&str> {
        file.waiters
            .iter()
            .map(|claim| claim.session_name.as_str())
            .collect()
    }

    /// Requests are granted first in, first out, each with a fresh deadline, and only once the
    /// previous holder's processes are gone.
    #[test]
    fn the_queue_is_granted_in_order_once_the_fence_is_free() -> eyre::Result<()> {
        let (one, two, three) = (holder("one"), holder("two"), holder("three"));
        let mut file = ResourceFile::default();
        file.admit("desk", &one, NOW, true, Request::default(), || {
            "one-claim".to_string()
        })?;
        queued(&mut file, &two, NOW)?;
        queued(&mut file, &three, NOW + 10)?;
        assert_eq!(waiter_names(&file), ["two", "three"]);

        // The holder gives the resource up while its processes still hold the fence.
        file.remove(&one);
        let later = NOW + 5_000;
        file.normalize(later, everyone_lives, false);
        assert_eq!(owner_name(&file), None);

        // Once the fence is free, the head of the queue gets it with a deadline counted from now.
        file.normalize(later, everyone_lives, true);
        assert_eq!(owner_name(&file), Some("two"));
        assert_eq!(
            file.owner
                .as_ref()
                .map(|owner| (owner.granted_at_unix_ms, owner.claim.renew_by_unix_ms)),
            Some((later, renew_by(later)))
        );
        assert_eq!(waiter_names(&file), ["three"]);
        Ok(())
    }

    /// A claim of a dead instance goes at once, a waiter past its deadline goes too, and an
    /// expired grant stays for its holder to end.
    #[test]
    fn dead_and_expired_claims_are_dropped() -> eyre::Result<()> {
        let (one, two, three) = (holder("one"), holder("two"), holder("three"));
        let mut file = ResourceFile::default();
        file.admit("desk", &one, NOW, true, Request::default(), || {
            "one-claim".to_string()
        })?;
        queued(&mut file, &two, NOW)?;
        queued(&mut file, &three, NOW + 1)?;

        // Past every deadline, but everyone alive: the grant stays, both waiters lapse.
        let expired = renew_by(NOW) + 1;
        let mut lapsed = file.clone();
        lapsed.normalize(expired, everyone_lives, false);
        assert_eq!(owner_name(&lapsed), Some("one"));
        assert_eq!(waiter_names(&lapsed), Vec::<&str>::new());

        // The holder's instance died: the next live waiter gets the resource.
        file.normalize(NOW, |claim| claim.session_name != "one", true);
        assert_eq!(owner_name(&file), Some("two"));
        assert_eq!(waiter_names(&file), ["three"]);
        Ok(())
    }

    /// Admission names why a session may not start the resource's services.
    #[test]
    fn admission_tells_the_reasons_apart() -> eyre::Result<()> {
        let (one, two) = (holder("one"), holder("two"));
        let mut file = ResourceFile::default();

        // Free, but the previous holder's processes still run.
        assert!(matches!(
            file.admission("desk", &one, NOW, false),
            Err(ReservationError::Stopping { .. })
        ));
        assert_eq!(
            file.admit("desk", &one, NOW, true, Request::default(), || {
                "one-claim".to_string()
            })?,
            renew_by(NOW)
        );

        // Another session holds it, and the message names the holder and how to queue.
        let held = file.admission("desk", &two, NOW, true);
        let Err(err @ ReservationError::Held { .. }) = held else {
            eyre::bail!("expected the resource to be held, got {held:?}");
        };
        let message = err.to_string();
        assert!(
            message.starts_with("resource `desk` is held: queue with `micromux ctl reserve desk`"),
            "{message}"
        );
        assert!(
            message.contains("held by session one (/projects/one)"),
            "{message}"
        );

        // A renewal moves the deadline, and one naming another request is refused.
        file.renew("desk", &one, NOW + 1_000, Some("one-claim"))?;
        assert_eq!(
            file.owner
                .as_ref()
                .map(|owner| owner.claim.renew_by_unix_ms),
            Some(renew_by(NOW + 1_000))
        );
        assert!(matches!(
            file.renew("desk", &one, NOW, Some("older-claim")),
            Err(ReservationError::Gone { .. })
        ));
        assert!(matches!(
            file.renew("desk", &two, NOW, None),
            Err(ReservationError::Gone { .. })
        ));
        Ok(())
    }

    /// Only the holder judges its deadline: past it, the holder goes on and renews while nobody
    /// waits, and is refused once somebody does, in the same transaction that sees the waiter.
    #[test]
    fn a_holder_past_its_deadline_lapses_only_when_contested() -> eyre::Result<()> {
        let (one, two) = (holder("one"), holder("two"));
        let mut file = ResourceFile::default();
        file.admit("desk", &one, NOW, true, Request::default(), || {
            "one-claim".to_string()
        })?;
        let overdue = renew_by(NOW) + 1;

        // Uncontested, a late holder keeps its grant: admission, reserve and renew agree, and a
        // late renewal extends it.
        assert_eq!(
            file.admission("desk", &one, overdue, true)?,
            Some(renew_by(NOW))
        );
        file.renew("desk", &one, overdue, None)?;
        assert_eq!(
            file.owner
                .as_ref()
                .map(|owner| owner.claim.renew_by_unix_ms),
            Some(renew_by(overdue))
        );

        // A waiter makes the next overdue check refuse the holder, which keeps the file as it
        // is until it removed itself.
        let later = renew_by(overdue) + 1;
        queued(&mut file, &two, later)?;
        let contested = file.clone();
        for refused in [
            file.admission("desk", &one, later, true).map(|_| ()),
            file.renew("desk", &one, later, None),
            file.enqueue(
                "desk",
                &one,
                later,
                Request::default(),
                Existing::Keep,
                String::new,
            ),
        ] {
            assert!(
                matches!(refused, Err(ReservationError::Lapsed { waiting: 1, .. })),
                "{refused:?}"
            );
        }
        assert_eq!(file, contested);
        file.remove(&one);
        file.normalize(later, everyone_lives, true);
        assert_eq!(owner_name(&file), Some("two"));
        Ok(())
    }

    /// A reserve that gives no estimate or reason keeps the ones the request has.
    #[test]
    fn reserving_again_keeps_the_estimate_and_reason() -> eyre::Result<()> {
        let one = holder("one");
        let mut file = ResourceFile::default();
        let request = Request {
            estimate_secs: Some(600),
            reason: Some("demo".to_string()),
        };
        file.enqueue("desk", &one, NOW, request, Existing::Update, || {
            "one-claim".to_string()
        })?;
        file.enqueue(
            "desk",
            &one,
            NOW + 1,
            Request::default(),
            Existing::Update,
            String::new,
        )?;
        let claim = file.waiters.first().cloned();
        assert_eq!(
            claim.map(|claim| (claim.estimate_secs, claim.reason)),
            Some((Some(600), Some("demo".to_string())))
        );
        Ok(())
    }

    /// A request that joins keeps a claim the session has as it is, owner or waiter, so a
    /// second start neither moves it in the queue nor extends it.
    #[test]
    fn joining_again_keeps_the_claim_as_it_is() -> eyre::Result<()> {
        let (one, two) = (holder("one"), holder("two"));
        let mut file = ResourceFile::default();
        file.admit("desk", &one, NOW, true, Request::default(), || {
            "one-claim".to_string()
        })?;
        queued(&mut file, &two, NOW)?;
        let unchanged = file.clone();
        let later = Request {
            estimate_secs: Some(600),
            reason: Some("later".to_string()),
        };
        for me in [&one, &two] {
            file.enqueue(
                "desk",
                me,
                NOW + 120_000,
                later.clone(),
                Existing::Keep,
                String::new,
            )?;
        }
        assert_eq!(file, unchanged);
        Ok(())
    }

    /// A yielded grant is due at once and goes on while nobody waits, a second yield keeps it
    /// due, and a waiter makes the holder's next check refuse it.
    #[test]
    fn a_yielded_grant_passes_on_once_a_request_waits() -> eyre::Result<()> {
        let (one, two) = (holder("one"), holder("two"));
        let mut file = ResourceFile::default();
        file.admit("desk", &one, NOW, true, Request::default(), || {
            "one-claim".to_string()
        })?;
        file.yield_turn(&one, NOW + 10);
        file.yield_turn(&one, NOW + 20);
        assert_eq!(
            file.admission("desk", &one, NOW + 30, true)?,
            Some(NOW + 10)
        );
        queued(&mut file, &two, NOW + 40)?;
        assert!(matches!(
            file.admission("desk", &one, NOW + 50, true),
            Err(ReservationError::Lapsed { waiting: 1, .. })
        ));
        Ok(())
    }

    /// A session keeps its queued request alive, and queues it again at the end of the queue once
    /// it lapsed anyway.
    #[test]
    fn a_lapsed_request_is_queued_again_at_the_end() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path().join("registry"));
        let range = super::super::DEFAULT_PORT_RANGE;
        let session = |name: &str| -> eyre::Result<_> {
            let config = PathBuf::from(format!("/projects/{name}/micromux.yaml"));
            let (_, lease) = registry.acquire(&config, name, &[], &[], &range)?;
            let store = ResourceStore::new(
                &registry,
                Holder {
                    session_id: super::super::config_path_key(&config),
                    instance: lease.token.clone(),
                    session_name: name.to_string(),
                    config_path: config.display().to_string(),
                },
            )?;
            Ok((lease, store))
        };
        let (_one_lease, one) = session("one")?;
        let (_two_lease, two) = session("two")?;
        let (_three_lease, three) = session("three")?;
        one.admit("desk", Request::default())?;
        two.reserve("desk", Request::default())?;
        three.reserve("desk", Request::default())?;

        // The request of `two` lapses, as across a long suspend.
        let path = directory.path().join("registry/resources/desk.json");
        let mut file: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        if let Some(renew_by) = file.pointer_mut("/waiters/0/renew_by_unix_ms") {
            *renew_by = serde_json::json!(1);
        }
        std::fs::write(&path, serde_json::to_vec(&file)?)?;

        let reply = two.keep_queued("desk")?;
        assert_eq!(reply.state, ReservationState::Waiting { position: 2 });
        assert_eq!(
            reply
                .view
                .waiters
                .iter()
                .map(|claim| claim.session_name.as_str())
                .collect::<Vec<_>>(),
            ["three", "two"]
        );
        Ok(())
    }

    /// A waiter's renewal moves its deadline only by a minute or more, so polling does not
    /// rewrite the file.
    #[test]
    fn waiter_renewals_skip_small_moves() -> eyre::Result<()> {
        let (one, two) = (holder("one"), holder("two"));
        let mut file = ResourceFile::default();
        file.admit("desk", &one, NOW, true, Request::default(), || {
            "one-claim".to_string()
        })?;
        queued(&mut file, &two, NOW)?;

        let unchanged = file.clone();
        file.renew("desk", &two, NOW + MIN_WAITER_RENEWAL_MS - 1, None)?;
        assert_eq!(file, unchanged);
        file.renew("desk", &two, NOW + MIN_WAITER_RENEWAL_MS, Some("two-claim"))?;
        assert_eq!(
            file.waiters.first().map(|claim| claim.renew_by_unix_ms),
            Some(renew_by(NOW + MIN_WAITER_RENEWAL_MS))
        );
        Ok(())
    }

    /// Reasons and queues are bounded.
    #[test]
    fn reasons_and_queues_are_bounded() -> eyre::Result<()> {
        let one = holder("one");
        let mut file = ResourceFile::default();
        file.admit("desk", &one, NOW, true, Request::default(), || {
            "one-claim".to_string()
        })?;

        let too_long = Request {
            estimate_secs: None,
            reason: Some("x".repeat(MAX_REASON_CHARS + 1)),
        };
        assert!(matches!(
            file.enqueue(
                "desk",
                &holder("two"),
                NOW,
                too_long,
                Existing::Update,
                String::new
            ),
            Err(ReservationError::ReasonTooLong)
        ));

        for index in 0..MAX_WAITERS {
            queued(&mut file, &holder(&format!("waiter{index}")), NOW)?;
        }
        assert!(matches!(
            file.enqueue(
                "desk",
                &holder("late"),
                NOW,
                Request::default(),
                Existing::Keep,
                String::new
            ),
            Err(ReservationError::QueueFull { .. })
        ));
        Ok(())
    }

    /// The file is shared by real sessions of one registry: a holder blocks the other session,
    /// a release hands the resource to the waiter once the fence is free, and a restarted
    /// session does not inherit its dead instance's grant.
    #[test]
    fn sessions_share_reservations_through_the_registry() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let registry = PortRegistry::at(directory.path().join("registry"));
        let range = super::super::DEFAULT_PORT_RANGE;
        let session = |config: &str, name: &str| -> eyre::Result<_> {
            let config = Path::new(config);
            let (_, lease) = registry.acquire(config, name, &[], &[], &range)?;
            let store = ResourceStore::new(
                &registry,
                Holder {
                    session_id: super::super::config_path_key(config),
                    instance: lease.token.clone(),
                    session_name: name.to_string(),
                    config_path: config.display().to_string(),
                },
            )?;
            Ok((lease, store))
        };
        let (one_lease, one) = session("/projects/one/micromux.yaml", "one")?;
        let (two_lease, two) = session("/projects/two/micromux.yaml", "two")?;

        // Looking at a free resource claims nothing and writes nothing.
        one.written_view("desk")?;
        assert!(
            !directory
                .path()
                .join("registry/resources/desk.json")
                .exists()
        );

        // The first session takes the resource; the second is refused and queues.
        one.admit("desk", Request::default())?;
        assert!(matches!(
            two.admit("desk", Request::default()),
            Err(ReservationError::Held { .. })
        ));
        let waiting = two.reserve("desk", Request::default())?;
        assert_eq!(waiting.state, ReservationState::Waiting { position: 1 });

        // While a process of the first session holds the fence, its release hands nothing on.
        let fence = one.lock_fence("desk")?;
        one.remove("desk")?;
        assert_eq!(
            two.poll("desk")?.state,
            ReservationState::Waiting { position: 1 }
        );
        drop(fence);
        assert_eq!(two.poll("desk")?.state, ReservationState::Granted);

        // The first session queues again after a restart of its own.
        drop(one_lease);
        let (_one_lease, one) = session("/projects/one/micromux.yaml", "one")?;
        one.reserve("desk", Request::default())?;

        // A restart of the second session's config replaces the holding instance: its token no
        // longer matches the grant, which passes to the first session.
        drop(two_lease);
        let (_restarted_lease, restarted) = session("/projects/two/micromux.yaml", "two")?;
        assert_eq!(one.poll("desk")?.state, ReservationState::Granted);
        assert_eq!(restarted.poll("desk")?.state, ReservationState::Released);
        Ok(())
    }
}
