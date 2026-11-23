# 7. A reservation is a request to run: the claim follows what the session wants to run, a grant starts it, and a release yields only to a waiter

**Status:** accepted · **Date:** 2026-11-23 · supersedes [ADR 6](0006-shared-resource-reservations.md)

## Context

[ADR 6](0006-shared-resource-reservations.md) let one session at a time run the services that name a resource.
In practice, worktrees that share a desktop app through `resource: desktop-app` were left without it (issue #101):

- A hand-over needed an agent. `enable` was refused while another session held the resource, and an automatic start that was refused disabled the service, so the next holder ran nothing until an agent noticed the grant and enabled the app.
- A release stopped the app even when nobody else wanted it, because it disabled the holder's services before it looked at the queue.

Both come from one cause: the ledger's claim and the services' intent to run were two records of the same fact, kept in step only by the order of calls in each command.
`apply_enable` took the resource before it recorded the start, so a refusal dropped the intent; `release` destroyed the intent before it asked whether anyone waited.

## Design

- **The claim follows the session's need.** A session claims resource `R` exactly while one of its services needs it: a live run captured `R` (retired services included), or a service whose definition names `R` has a start to come.
  One predicate, `has_pending_start`, decides a start to come for scheduling, for the claim, for the reload pin and for the `waiting_for_resource` flag: enabled, and a requested start, a first start of a service that starts with the session, or a restart its policy schedules; a requested restart behind a live run counts, a dynamic service whose lease ran out does not.
- **Claims are given up at one boundary.** `start_ready`, after commands committed what they requested, removes every waiting or granted claim nothing needs any more; a pass that did so ends there and another follows (B7).
  Process events handled during a store await never remove a claim or spawn.
- **Ledger, liveness and fence are unchanged** from ADR 6: one JSON file per resource under the registry lock, claims of dead instances dropped by every transaction, and the fence that keeps a successor out until the previous holder's processes and cleanups are gone. The file format is unchanged, so sessions of both binaries still exclude each other.
- **Protocol 4.0.** `Enable` queues, `Reserve` requests starts, and `ReleaseReservation` keeps the services running while nobody waits; these change existing request semantics, so the control protocol is bumped to 4.0 under [ADR 2](0002-control-protocol-stays-backward-compatible.md). Every 4.x session answers every request, so the 3.x revision gates and their fallbacks are gone. `ReservationStatus` is new.

## Decisions

Each entry gives the decision, why, what was rejected, and what would have to change to revisit it.

### B1. Named starts queue; bulk, dynamic and autonomous starts take only a free resource

`enable SERVICE` (not of a job), `restart SERVICE` and `reserve` take the resource or join the queue, and the start is committed in the same scheduler turn; an existing claim, granted or queued, is used as it is, without moving it or extending its deadline. A queued claim is confirmed in the store first, since it may have lapsed while the machine slept.
`restart-all`, a dynamic start or replacement, and the starts a session makes on its own (session startup, reconcile additions, automatic restarts) take only a free resource, as ADR 6 A7 did: a refusal changes nothing, and an automatic start that is refused disables the service with one event, whose text says how to queue. `restart-all` leaves a start that waits for its resource waiting, since it has no run to restart and starts with its current definition once granted.
A start waits, without being disabled, while the session's request is queued.

- *Why:* a queued claim is visible to other sessions at once and can make a yielded holder hand over. A command over several resources that queued for one and was then refused for another would have made that holder stop for nothing. A dynamic service's lease would run while it waits, so a short one could expire without ever running.
- *Rejected:* queueing every explicit start; refusing named starts as ADR 6 did.
- *Revisit* if agents need `restart-all` or dynamic starts to queue.

### B2. A release yields: the grant becomes due and passes on only to a live waiter

Releasing a grant sets its `renew_by` to now, in the transaction that reads the queue. With a live waiter the holder hands over at once: it disables its services that need the resource, those with a live run of it or a start to come, refuses further starts and renewals, and removes its claim once their runs and cleanups are gone. A job whose run finished and a manual job that waits to be run keep their state, so services that depend on their outcome are not stranded. Without one, the claim, processes and run generations stay, and the reply still says `granted`.
Releasing a queued request disables the services whose starts waited for it and removes the claim.
To stop a broken copy the holder disables its services; disabling the last one gives the resource up.

- *Why:* nobody loses the app when nobody else wants it.
- *Rejected:* stopping on release, as ADR 6 A2 did.
- *Revisit* if a holder needs to stop and give up the resource with one command.

### B3. The holder judges its deadline, now on the two-second poll

The holder's `renew_by` stays the one deadline, judged only by the holder (ADR 6 A1): before it, an hour after the grant, the turn is protected; past it, the holder goes on while nobody waits and hands over once somebody does. Renewing extends an uncontested grant, a released one included; once a hand-over is due with a live waiter, renewing is refused.
An overdue grant is judged again every two seconds, so a waiter that arrives after a release gets the resource within seconds plus the holder's teardown. Before the deadline the scheduler only looks at the wall clock, at most a minute apart.
A waiter that cancelled, died or expired before the check causes no stop; a hand-over that began completes even if the last waiter cancels, and leaves the resource free.

- *Rejected:* a 30-second rejudge, which made a released app linger long after somebody queued.

### B4. The session keeps its own queued request alive; waits only look

The session renews its queued claim on its own two-second poll, throttled to one write a minute, so a request made by `enable` does not lapse while no agent waits.
`ctl wait --reservation` and MCP `wait_for_reservation` send `ReservationStatus`, which reads the file as last written plus the session's local hand-over state; they never renew a request or extend a grant, pin the session instance and the claim id, and report ownership. Readiness is a service wait.
A queued request that lapsed anyway, which takes an hour without the session's poll, as across a long suspend, is queued again at the end of the queue by the next poll or named start, with the starts that waited for it.

- *Why:* the issue's hand-over must need no agent; and a wait that extended a grant undid a release.
- *Rejected:* the agent's wait renewing the request, as in ADR 6, which let an `enable` expire after an hour while its agent did other work.
- *Consequence:* a request outlives the agent that made it; a worktree whose agent is gone can get the app for its protected hour.

### B5. A fresh reservation requests the group that starts with the session

`reserve R` without a claim requests a start of the live configured services that name `R` and start with the session: not one with `disabled: true`, not a manual job; a job keeps the outcome of a run that finished on its own, and a dynamic service is never part of it, though a pending start of one runs on a grant.
A reservation that would start nothing is refused before anything changes. A repeated reservation renews the claim and takes the estimate and reason, and enables nothing, so a service disabled during the turn stays disabled.

- *Rejected:* reserving a resource without work behind it, which only kept others waiting.

### B6. A session hands over one turn at a time

While a session hands a resource over, `enable`, `restart`, `reserve` and `renew` of it are refused with the hand-over named; once the claim is gone, a named start queues at the tail.

- *Rejected:* accepting a start for after the hand-over, which needs two turns of one session for one resource and a rule for when the second enters the queue.

### B7. A scheduling pass ends at its first store transaction, after the commands already queued

Each scheduling pass first handles the commands that are in the scheduler's channel when it begins, then starts what is ready, and ends at its first store transaction: taking a resource for a ready service, or giving up claims nothing needs. A pass that ended there makes another one due, which wakes the scheduler like a timer, so timers and commands get their turns in between; each command is acknowledged once the pass after it ran.
So a disable that waited while the poll noticed a grant, or while the pass took another resource, applies before the grant starts anything: the service never spawns, and its cleanup never holds the fence against the next session.

- *Known limitation:* a command that arrives while another command of the same batch awaits the store applies with the next batch, after the pass that may start what it would have turned off. The fence keeps the hand-over safe; the cost is one teardown of delay for the next session.
- *Rejected:* draining the channel until it is empty, which lets a stream of commands hold back the passes and every reply; and serializing the senders with the scheduler's start decision through a lock and command slots held until a pass completes, which closes the window above but adds shared state between every client and the scheduler, and a deadlock hazard, for a window of milliseconds.

### B8. Store transactions run to their end, and a stopping session gives idle claims up first

A store transaction is not abandoned when the session starts to stop; the registry lock's timeout bounds it, so a claim it writes is one the session knows.
As the session stops, it first removes its queued requests and the grants no live run needs, so no holder hands a resource over to it, and removes the rest once its services stopped.

### B9. A resource stays pinned on a live run or a start to come

A reload or reconcile that adds, removes or changes the `resource` of a service with a live run or a start to come is refused until it is disabled and torn down.
A dynamic clone keeps its inherited resource and fence, and a hand-over disables it with the services of its resource.

### B10. Observability

`ServiceSnapshot.waiting_for_resource` tells an accepted start that waits for the session's turn from a dependency block or a disabled service; the TUI shows it as `QUEUED`. A start request's event says when it waits, `ResourceGranted` records the grant on each service that waited, and a hand-over disables with an event saying a request waits. An uncontested release replies that the session still holds the resource and hands it over once another session queues.

### Carried over from ADR 6

- **Scheduler-side authorization at commit** (A8), **the caller on every command**, and **`Identify`** on the connection that carries mutations: only the own project controls a service that names a resource.
- **Claim ids are stable and renewals may name one** (A3).
- **A foreign shutdown is one bounded scheduler command** that refuses while the session holds, waits for, or hands over a resource, or a tagged run has not ended (A4).
- **Bounds:** 16 waiters, reasons of 200 characters, `reservations_truncated` (A5).
- **Terminal input bound to the authorized run** (A9), **the supervisor hands the fence to the cleanup-only shim** (A10), and **a command's reload authorized like a reconcile** (A11).
- **Unidentified clients are refused protected operations** (U2), and **the TUI has no keys to reserve, renew or release** (U3); enabling a service in the TUI queues like `ctl enable`.
- **Strict first in, first out; release plus FIFO only; a fixed one-hour turn; no proxy routing; cooperative same-user scope; transactions with `spawn_blocking`.**
- **Reservations are Unix-only**, like routes ([ADR 5](0005-proxy-and-routes-are-unix-only.md)).

## Consequences

- A session asks for a resource once, by enabling a service or reserving; the grant starts what waited, after the previous holder's processes and cleanups are gone.
- A released app keeps running until somebody queues.
- A refused `restart-all` keeps no grant it took, since the pass after it gives up every claim nothing needs.
- Protocol 4.0 orphans every running 3.x session: clients refuse it, and it must be restarted with the new binary.
- During a rolling upgrade, an old holder still stops on release and an old waiter still needs an agent to enable its service after the grant; a new holder hands over to an old waiter like to any other.

## References

- `crates/micromux/src/scheduler/schedule.rs`: `has_pending_start`, the waiting start in `start_service_if_ready`, `record_resource_wait`
- `crates/micromux/src/scheduler/reservations.rs`: `queue_for`, `admit`, `needs_resource`, `give_up_claims`, `start_ready`, `release`, `reserve`, `reservation_group`, `track`, `remove_idle_claims_on_stop`
- `crates/micromux/src/scheduler.rs`: the run loop's command drain, `apply_enable`, `apply_restart`, `apply_restart_all`, `reject_resource_changes_of_started_services`, `await_handling_events`
- `crates/micromux/src/session/reservations.rs`: `join`, `reserve`, `yield_turn`, `poll`, `status`, `judge`
- `crates/micromux-control/src/protocol.rs`: `PROTOCOL_VERSION`, `Request::ReservationStatus`
- `crates/micromux-control/src/wait.rs`: `wait_for_reservation`
- `crates/micromux/src/scheduler/tests.rs` (`reservations`) and `crates/micromux-cli/tests/containment.rs`: the hand-over tests
- `docs/content/docs/configuration/services.md` and `docs/content/docs/agent-control/control-plane.md`, "Shared resources"
- Issues #99 and #101
