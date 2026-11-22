# 6. Shared resources: one session at a time runs a service that names a resource, judged by its holder

**Status:** accepted · **Date:** 2026-11-23

## Context

Sessions of different worktrees run side by side, but some services must run only once per machine: a desktop app on a fixed port, a GPU-bound model server, a device.
Issue #99 asks for a reservation that one session at a time holds, with a queue, an estimate and a reason others can read, and a way for agents to wait for it.

The design has to survive what sessions do in practice: they crash, get `SIGKILL`ed, sleep with a suspended laptop, and are driven by agents in other worktrees whose `micromux ctl` reaches any session.
It must never let two sessions run the resource's services at once, and must never wedge a resource behind a session that is gone.

## Design

- **Ledger.** `<registry>/resources/<R>.json` records the owner and a first-in-first-out queue of claims, each with a stable claim id, the session id, the instance token, an estimate, a reason, and a wall-clock `renew_by`.
  Every change is one transaction under the port registry's lock, run with `spawn_blocking` off the scheduler.
  The file is replaced atomically, so displays read it without the lock (`ResourceStore::written_view`).
- **Instance liveness.** A claim is live while its session holds its instance lock and its port record still names the claim's instance token.
  Each transaction drops dead claims and expired waiters, so a crashed holder never wedges a resource and a restarted session never inherits its old instance's claims.
- **Fence.** `<registry>/resources/<R>.fence` is locked shared for every run of a service that names `R`: by the supervisor from before the spawn until the run exited, by the run's shim through its own teardown, and by the cleanup-only shim, which inherits the supervisor's locked descriptor as its stdout, until the cleanup ended.
  A waiter is promoted only when an exclusive try-lock of the fence succeeds, so the next holder starts only once the previous holder's processes and cleanups are gone, however the previous holder ended.
- **Holder-judged contested lapse.** The ledger's `renew_by` is the one deadline. Only the holder judges it, inside the transaction that admits, reserves, or renews for it: before the deadline it goes on; past it, it goes on while nobody waits; past it with a live waiter it is refused and releases.
  The scheduler's timer is only a wake-up to look at the wall clock and, once overdue, ask the store again.
- **Scheduler-side authorization.** Every mutation carries a `Caller` (`Local`, `Project(id)`, `Unknown`) to the scheduler, which authorizes it at commit, against the live run and the definition or reconcile actions about to be committed. Only the own project controls a service that names a resource.
- **Identify.** A client names its project, the config found at or above its working directory, on the connection that carries its mutations (`Request::Identify`, control protocol 3.21), right after a `Describe` on that same connection (`Client::describe_identified`).

## Decisions

Each entry gives the decision, why, what was rejected, and what would have to change to revisit it.

### A1. Contested lapse, judged by the holder (with U1)

The ledger keeps no "lapsed" state and no other session expires a holder. In the transaction that admits, reserves, or renews for the holder, after pruning dead and expired waiters: not overdue → proceed; overdue with no live waiter → proceed, and a late renewal extends the grant; overdue with a live waiter → refuse and begin releasing.
Before the deadline the scheduler only wakes, at most a minute apart, to look at the wall clock; once it is overdue it has the store judge the grant, and an overdue, uncontested grant is judged again every 30 seconds, so a waiter that arrives later is noticed.

- *Why:* one clock and one judge. After a suspend, `renew`, `reserve` and starts all ask the same transaction and agree; nobody loses a grant nobody else wanted.
- *Rejected:* a strict lapse at `renew_by` judged by the scheduler's monotonic clock (it disagreed with the ledger after a suspend, and forced humans to renew uncontested grants); waiters expiring the holder (a second judge that races the holder's own runs).
- *Revisit* if a resource needs a hard time limit regardless of demand.

### A2. Release keeps a local `Releasing` state and one idempotent removal

On release or contested lapse the scheduler disables the resource's services, refuses further starts and renewals, and acknowledges at once ("releasing"). The ordinary owner stays in the file until the tagged runs ended, then one idempotent `remove` takes it out; a queued claim is removed at once. A failed removal keeps `Releasing` and is retried on the poll interval.

- *Rejected:* a persisted `Draining` owner state with a preliminary write (a second source of truth other sessions had to interpret, and one more write that can fail); `removal_pending`/`retry_pending` flags (the poll interval already retries).
- *Revisit* if other sessions need to see that a holder is releasing; today only the holder's own replies say "being released".

### A3. Claim ids and pinning stay

A claim id is stable through renewal and promotion, a renewal naming another id is refused, and `wait --reservation` pins the id its first renewal reports.

- *Why:* a late renewal from a waiter of an earlier request must not extend a newer one.
- *Rejected:* removing claim ids and matching by session only.
- *Revisit* never without a replacement for that check.

### A4. A foreign shutdown is one bounded scheduler command

Another project's `Shutdown` is a scheduler command, bounded by `REQUEST_TIMEOUT`, that refuses while the session holds, waits for, or releases a resource or a tagged run has not ended (a retired one included), and otherwise cancels the session in the same scheduler turn, before it replies, so nothing is admitted or started after it. The server then writes `ShuttingDown` under a token of its own. A configured tag alone counts for nothing. The own project stops the session directly, with no scheduler round trip, so it works against a wedged scheduler. `watched_resources` is for display only.

- *Rejected:* deciding from a published snapshot of used resources (stale by design, and it let a foreign stop slip through during startup); counting configured tags (it protected sessions that hold nothing).
- *Revisit* if stopping must be possible while the scheduler is wedged for foreign callers too.

### A5. Bounds stay

At most 16 waiters per resource, reasons of at most 200 characters, and a description too large to send drops its reservations and sets `reservations_truncated`, which `ctl`, MCP and the TUI surface instead of an empty list.

- *Rejected:* removing the bounds (one misbehaving agent could grow the file and every description without limit).
- *Revisit* with real queues longer than 16.

### A6. A refused `RestartAll` reports its first typed rejection

`RestartAll` authorizes its whole restart set, then admits its distinct resources in order before restarting anything. The first refusal is returned with its own error code, plus the resources admitted before it ("the session holds A"), which the session keeps.

- *Rejected:* inferring which grants the command itself created, releasing them again, and rescanning deadlines after admission (machinery for a rare case whose only cost is a grant held that the message names).
- *Revisit* if partial grants turn out to block other sessions in practice.

### A7. Explicit starts admit at command time, autonomous starts at readiness

`restart`, `enable` (not of a job), `restart-all`, a dynamic start and a replacement admit the resource of the definition that will run before committing anything; a refusal changes nothing, and a dependency-blocked start still takes the resource. Session startup, reconcile additions and automatic restarts are admitted inside the dependency-readiness traversal: once a service is ready, the scheduler admits, re-checks readiness and shutdown after the store await, and spawns under the grant; a refusal disables the service with one event. Every spawn checks the live grant.

- *Rejected:* eager admission at enable time for autonomous starts (it took resources for services that might never become ready); a second fixpoint loop with a `needed_resources` out-parameter (it duplicated refusal events).
- *Revisit* if a service must hold its resource while it waits for dependencies without an explicit command.

### A8. Authorization at commit, in the scheduler

The caller travels with every command and the scheduler authorizes against what it is about to commit: the live run, the definition, the reconcile actions.

- *Rejected:* edge-only authorization in the control server (it checked a snapshot that could change before the command committed).
- *Revisit* never; any new mutation carries its caller.

### A9. Terminal input is bound to the authorized run

Input the control server accepts for a service carries the run generation it was authorized for, and the scheduler discards it at delivery when the service's current run differs.

- *Why:* a replacement between authorization and delivery must not receive another project's keystrokes.

### A10. The supervisor holds the fence and hands it to the cleanup

For a tagged run the supervisor locks the fence before the spawn, holds it until the run exited, and gives the locked descriptor to the cleanup-only shim as its stdout (an inherited open file description, so the lock outlives the supervisor). The shim never writes to it, and its cleanup command gets the log as stdout, so nothing else inherits it. A run whose fence cannot be locked fails to start. The service shim still locks by path, since its PTY spawn closes inherited descriptors. Reservations are Unix-only, like routes ([ADR 5](0005-proxy-and-routes-are-unix-only.md)): `resource` is a config error where there is no containment.

- *Rejected:* the cleanup-only shim locking the fence by path on its own (a supervisor dying between spawning the shim and its lock let the resource pass on while the cleanup ran); passing the descriptor with a new dependency such as `command-fds`, which needs a descriptor number in the child and `unsafe` there to claim it; reservations without containment (nothing would stop a dead holder's services).
- *Revisit* when Windows gains containment.

### A11. A command's reload is authorized like a reconcile

`restart`, `enable` and `restart-all` reload the config first; every changed protected definition in that reload is authorized with the reconcile rule before it is published.

- *Why:* otherwise a foreign restart of an unprotected service could publish an edit of a protected one.

### U1. Contested lapse replaces strict lapse

See A1.

### U2. Clients that name no project are refused protected operations

A client older than control protocol 3.21 never identifies, nor does one outside any project, so it is refused for protected operations with "this connection did not name its project; run a micromux with control protocol 3.21 or newer from inside <project dir> (a client outside any project names none)". Older clients cannot control tagged services or stop a session that holds a resource.

- *Rejected:* trusting unidentified clients (any old `ctl` in any worktree could take a resource away).

### U3. The TUI gets no keys to reserve, renew or release

A human driving the TUI alone holds the resource uncontested, and with contested lapse an uncontested grant keeps running past `renew_by`, so nothing forces them to renew. Queueing, renewing and releasing are for agents and scripts, which use `micromux ctl` and the MCP tools. A refused start in the TUI shows its message in the header, leading with the `ctl reserve` next step so it fits a normal terminal width; the holder follows when there is room, and the session overview (`u`) always shows it.

- *Revisit* when humans need to queue for or hand over a resource from the TUI, or when the lapse rule stops being contested.

### Earlier settled decisions

- **Strict first in, first out.** The queue has no priorities and no jumping. *Rejected:* priorities or preemption, which need policy nobody asked for. *Revisit* with a concrete need for priorities.
- **Release plus FIFO only.** A holder releases; it cannot hand the resource to a named session. *Rejected:* named hand-over, which bypasses the queue. *Revisit* if agents need to pass a resource within one task.
- **A fixed one-hour renewal interval**, with no config key. *Rejected:* a configurable interval, a knob without a user. *Revisit* when a workload needs another interval.
- **No proxy routing.** A resource does not route or proxy anything; its services keep their ports. *Rejected:* routing a resource's hostname to its holder. *Revisit* if users want a stable URL for whichever session holds the resource.
- **Cooperative, same-user scope.** The ledger lives in the user's registry and authorization keeps sessions from taking each other's resources by accident; it is no security boundary against the same user.
- **Transactions with `spawn_blocking`.** The registry lock is a blocking file lock, so every transaction runs off the async executor. The scheduler awaits it the way it awaits a config load, so it handles nothing else for the milliseconds the registry lock is held. *Rejected:* a separate reservation task, a second owner of the session's grants. *Revisit* if registry transactions ever take long enough to delay process events noticeably.

## Consequences

- Two sessions never run a resource's services at once, also across crashes, kills and suspends.
- A dead holder's claims vanish at the next transaction, but displays read the file as last written and can show them until then.
- A refused `restart-all` can leave grants held, which its message names.
- Older clients lose control over tagged services; the docs and the refusal say what to run instead.
- `resource` is refused on Windows.

## References

- `crates/micromux/src/session/reservations.rs`: the ledger, the fence, `judge`
- `crates/micromux/src/scheduler/reservations.rs`: `Standing`, `admit`, `check_due_grants`, `begin_release`, `settle_releases`, `shutdown_for`, `start_ready`
- `crates/micromux/src/scheduler/schedule.rs`: admission inside the readiness traversal, the fence lock at spawn
- `crates/micromux/src/scheduler/pty.rs` and `crates/micromux/src/cleanup.rs`: the held fence and its hand-over to the cleanup-only shim
- `crates/micromux/src/scheduler/input.rs`: input bound to a run
- `crates/micromux-control/src/server/unix.rs`: `Identify`, the shutdown path
- `crates/micromux-control/src/client.rs`: `Client::describe_identified`, `connect_identified`, `require_reservations`
- `crates/micromux-cli/tests/containment.rs`: the hand-over tests after a `SIGKILL`
- `docs/content/docs/configuration/services.md` and `docs/content/docs/agent-control/control-plane.md`, "Shared resources"
- Issue #99
