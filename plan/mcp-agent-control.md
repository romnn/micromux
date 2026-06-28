# Plan: agent control plane + MCP server for micromux

## Goal

Let a coding agent (Claude Code, Codex) **discover and control running micromux
sessions** — list services, read logs, restart/enable/disable, check health,
and wait for a service to become healthy — through a single MCP server that is
configured once, exactly like `playwright-mcp` or `perfetto-mcp`.

The agent should not have to rediscover PIDs, ports, and log files every
session, and — more importantly — its actions should go through the **same
control plane the human uses in the TUI**, so dependency gating, health
re-probing, and restart policy are respected. An agent restarting a service via
micromux is not just more convenient than `kill -9` + rerun, it is *more
correct*.

## Non-goals

- No new supervision logic. We surface state micromux already owns.
- No cross-machine / network exposure. Local, single-user, filesystem-scoped.
- No replacement for the TUI. The TUI and the agent are peer adapters over one model.
- Not a generic remote-exec surface. Tool surface stays small and declarative.
- **No raw input forwarding.** Every tool is typed/structured. We intentionally
  do *not* expose `Command::SendInput` (which writes raw bytes to a *service's*
  PTY stdin — not micromux's own TUI keys; the latter never pass through a
  `Command` at all). Keeping the surface typed avoids a stdin/keystroke channel.

---

## Architecture in one line

**One authoritative lifecycle model in the core; the TUI, the control socket,
and the MCP server are all adapters over it.** The scheduler already owns the
lifecycle truth — M0 surfaces it as a queryable model; later milestones add
adapters and fold the TUI onto the same model so there is exactly one source of
truth, not two reducers that can drift.

The core exposes that model as a **write capability** (scheduler-only) and a
**read capability** (every adapter), with commands flowing the other way through a
send-only port — capability security by Rust visibility, not convention. The core
knows nothing about sockets, pipes, MCP, JSON, or filesystem discovery; those live
entirely in `micromux-control` and `micromux-mcp`.

micromux is already split into a frontend-agnostic core and a command/event
interface (`crates/micromux/src/lib.rs`, `scheduler/types.rs`):

```
Micromux::start(ui_tx, commands_rx, shutdown)
   scheduler ──Event──▶  ui_tx        Started, LogLine, Healthy/Unhealthy,
                                       Exited, HealthCheck*, Disabled, ClearLogs
   scheduler ◀─Command── commands_rx  Restart, RestartAll, Enable, Disable,
                                       SendInput, ResizeAll
```

### Three facts from the current code that shape the design

1. **Lifecycle truth lives in the scheduler.** `ServiceRuntime` already holds
   both `desired` (Enabled/Disabled) and `state`
   (Pending/Starting/Running{health}/Disabled/Exited/Killed), and allocates a
   monotonic `RunId` per start. The model should be *written by the scheduler
   from this truth*, not re-derived by a second reducer.
2. **Service logs live in the TUI, not the core.** `micromux-tui/src/state.rs`
   holds `logs: AsyncBoundedLog` per service; `reducer::apply` materializes state
   from the event stream. The scheduler emits `LogLine` and forgets. The model
   must own the log buffer so any adapter can read it.
3. **Logs are already best-effort upstream.** The PTY reader thread sends lines
   to the scheduler with a bounded **`try_send`** (`scheduler/pty.rs` `send_log`),
   and interactive alt-screen snapshots are explicitly rate-limited and
   droppable. So end-to-end log delivery is best-effort *by design* — the PTY
   drain must never block. This bounds what "lossless" can honestly mean (see M0).

---

## Target topology

```
Claude / Codex ──stdio(JSON-RPC)──▶  micromux mcp     (thin proxy; read-only on disk, no supervision state)
                                          │  derives the socket path from cwd's config, or scans the socket dir
        ┌─────────────────────────────────┼──────────────────────────────────┐
        ▼                                  ▼                                   ▼
  $XDG_RUNTIME_DIR/micromux/        session A control endpoint         session B control endpoint
  ├─ a1b2c3.sock  ◀──────────────── (unix socket / named pipe;         (unix socket / named pipe)
  └─ d4e5f6.sock                     name = hash(config path))                ▲
                                            ▲  query + commands + Describe     │
                                   micromux (TUI) in proj-A          micromux (TUI) in proj-B
```

Two transports, deliberately different:

- **Agent ↔ MCP server: stdio.** MCP servers in agent configs are *spawned by
  the client* over stdio (not pre-existing endpoints the client dials). One
  process per agent session. This is why "every micromux is a server on a port"
  does not fit, and is where the port-conflict worry comes from.
- **MCP server ↔ micromux sessions: a local control endpoint** (Unix domain
  socket on Unix/macOS; named pipe on Windows), keyed by filesystem path / pipe
  name. N sessions never collide, no port allocation, user-permission scoped, no
  network. **The port-conflict problem disappears.** On Unix/macOS there is **no
  metadata registry — discovery uses sockets only**: the socket name is derived
  deterministically from the project's config path and all metadata is fetched live
  via `Describe`. (The only other on-disk file is a permanent per-hash `.lock` used
  *solely* for ownership coordination — never read for discovery or metadata.)
  Windows is not socket-enumerable, so it uses a per-hash sentinel file instead — see
  spec.

Rejected alternative — *each session is its own HTTP/SSE MCP server*: agent
config needs a stable URL; a per-session ephemeral port has none, reintroducing
the discovery + allocation problem the proxy already solves for free.

---

## Component breakdown

| Component | Crate | Responsibility |
|---|---|---|
| Authoritative session model | `micromux` (core) | Private `Inner`; possession-scoped `SessionModelWriter` + `Clone` `SessionModelReader`; narrow `ServiceControl` adapter for untrusted frontends; `SessionChange` notifications. The enabling refactor. |
| Control protocol + client/server | `micromux-control` (new) | Wire envelopes, `Describe`/discovery, concrete `ControlEndpoint` enum + client/server framing, path derivation. Shared by server + MCP. |
| Control endpoint adapter | `micromux-cli` | Bind the endpoint (race-safe dance) and clean up on exit; run `ControlServer` over a `Reader` + `ServiceControl` (no writer, no input-forwarding). |
| `micromux ctl` (optional) | `micromux-cli` | Tiny CLI client that dogfoods the socket; validates the protocol; nice for humans/scripts. |
| MCP server | `micromux-mcp` (new lib) + `micromux mcp` subcommand | Discover sessions, expose MCP tools, proxy to endpoints. |

Distribution **(decided)**: expose the MCP server as a **subcommand**
(`micromux mcp`) so a `brew install` / `cargo install` ships one binary and the
agent config is just `{"command": "micromux", "args": ["mcp"]}`. Keep the logic
in a `micromux-mcp` library crate; the subcommand is a thin shim. The whole thing
sits behind a **default-on `mcp` Cargo feature** so the heavy `rmcp` dependency is
only compiled when wanted — `cargo install --no-default-features` yields a lean
TUI-only binary with no MCP code linked in.

---

## Milestones

Each milestone is independently mergeable and leaves `main` shippable.

### M0 — Enabling refactor: the authoritative session model

**No user-visible behavior change for a responsive TUI; a wedged TUI no longer
stalls supervision.** The TUI is otherwise unchanged in this milestone. This is
the structurally important part: the model becomes the single source of truth, and
it is **written by the scheduler from its own state transitions** — not a second
reducer that independently re-derives lifecycle from thin events (which is what
would drift).

**Scheduler writes the model — by construction, not by discipline.** Today
lifecycle changes happen both via methods (`request_restart`, `disable`,
`finish_current_run`) *and* via direct `runtime.state = …` assignments in
`handle_event` (Healthy/Unhealthy/Killed). Two changes make "forgot to sync the
model" *impossible* rather than merely discouraged:

1. **Private state + transition methods that update runtime *and* model together.**
   `ServiceRuntime`'s `state`/`desired` become module-private; the *only* way to
   change them is a `SchedulerRuntime` transition method — `mark_started`,
   `mark_health`, `mark_killed`, `finish_run`, `request_restart`, `request_enable`,
   `disable` — and each one mutates the runtime and then calls a single private
   `write_snapshot(service_id)` that projects the runtime through the
   desired/execution table and writes it via the model **writer handle**. No
   separate "apply the delta" step to forget; no `runtime.state = …` anywhere else.
2. **Only the scheduler holds the writer** (capability-by-possession below), so it
   is *provably* the sole writer — adapters never possess a write handle.

`write_snapshot` is the single projection site, and the projection itself
(`&ServiceRuntime -> ServiceSnapshot`) is a **pure function**, unit-tested in
isolation from any lock or writer. For `ProcessEvent::LogLine` the scheduler calls
`writer.append_log(…)` **before** forwarding to the TUI (lossless into the model;
see below). Healthchecks go through the writer's **lifecycle methods**
(`start_health_attempt` → `append_health_line` → `finish_health_attempt`), mirroring
the existing `HealthCheckStarted`/`HealthCheckLogLine`/`HealthCheckFinished` events
so output ordering and the result update stay structurally explicit.

**Generation & uptime must survive exit.** `finish_current_run` currently does
`self.running.take()`, discarding the only `RunId` and start instant — so an
exited/disabled service could not report the generation that just ran, which would
make `restart → wait_for_healthy(after_generation = G)` ambiguous. Before dropping
`running`, record `last_run_id`, `last_started_at`, and `last_exit_code` on the
runtime. The model's `run_generation` = the current run's id if running, else
`last_run_id`; `uptime` = `now − last_started_at` while running.

**The model is the materialized truth, fed authoritatively:**

```rust
// crates/micromux/src/model.rs  (sketch)
pub enum Desired   { Enabled, Disabled }
pub enum Execution { Pending, Starting, Running, Stopping, Exited } // promoted from the TUI's domain state

pub struct ServiceSnapshot {
    pub id: ServiceID,
    pub name: String,
    pub desired: Desired,             // requested state (Disabled is a *desire*, not an execution)
    pub execution: Execution,         // observed lifecycle
    pub health: Option<Health>,       // reuses core health_check::Health; None until first probe resolves
    pub run_generation: u64,          // public name for scheduler RunId; bumps on every (re)start
    pub open_ports: Vec<u16>,
    pub healthcheck_configured: bool,
    pub last_exit_code: Option<i32>,
    pub uptime: Option<Duration>,     // since the current run's Started
    pub restart_policy: RestartPolicy,
}

// Private shared state. Neither handle exposes the lock; both wrap the same Arc<Inner>.
struct Inner { /* one RwLock<IndexMap<ServiceID, ServiceEntry>>:
                 snapshot + BoundedLog + live_snapshot_id + bounded HealthCheck history;
                 broadcast::Sender<SessionChange> */ }

/// READ capability. `Clone`, handed to every adapter (TUI, control server, MCP).
/// It has no write methods — the lack is the security boundary.
#[derive(Clone)]
pub struct SessionModelReader { inner: Arc<Inner> }
impl SessionModelReader {
    // snapshot/copy under the lock, drop it, THEN serialize — never hold the guard across .await:
    pub fn services(&self) -> Vec<ServiceSnapshot>;
    pub fn logs(&self, id: &ServiceID, tail: Option<usize>) -> Vec<LogLine>;
    pub fn healthchecks(&self, id: &ServiceID) -> Vec<HealthAttempt>; // get_health returns the latest
    pub fn subscribe(&self) -> broadcast::Receiver<SessionChange>;
}

/// WRITE capability — capability-by-possession, NOT by a restricted-visibility path.
/// (`pub(in crate::scheduler)` would NOT compile here: a restricted path must name an
/// *ancestor* module, and `crate::scheduler` is a sibling of `crate::model`.) Instead:
/// `SessionModelWriter` is `pub(crate)`, has NO public constructor and is `!Clone`, and
/// the ONLY way to obtain one is `SessionModel::new()` below — which hands it straight into the
/// scheduler future. It never appears in `Handles`, so no adapter can hold one.
pub(crate) struct SessionModelWriter { inner: Arc<Inner> }
impl SessionModelWriter {
    pub(crate) fn write_snapshot(&self, snap: ServiceSnapshot);     // publishes Change{Status}
    pub(crate) fn append_log(&self, id: &ServiceID, update: LogUpdateKind, line: String);
    // healthcheck lifecycle mirrors the scheduler's events (Started/LogLine/Finished),
    // so ordering and the result update stay structurally explicit:
    pub(crate) fn start_health_attempt(&self, id: &ServiceID, attempt: u64, command: String);
    pub(crate) fn append_health_line(&self, id: &ServiceID, attempt: u64, stream: OutputStream, line: String);
    pub(crate) fn finish_health_attempt(&self, id: &ServiceID, attempt: u64, success: bool, exit_code: i32);
}

/// The ONLY constructor for the model — returns the paired handles. `start_with_handles`
/// moves the writer into the scheduler future and returns only the reader.
impl SessionModel {
    pub(crate) fn new() -> (SessionModelReader, SessionModelWriter);
}

pub struct SessionChange { pub service_id: ServiceID, pub kind: ChangeKind } // Status | Logs | Health
```

**Capability flow.** The writer is unforgeable (no public constructor, `!Clone`) and
is moved into the scheduler future, never into `Handles` — so the reader is the only
model handle an adapter ever sees. The only path by which an adapter can affect state
is: *adapter → `ServiceControl` → scheduler → `Writer` → model → `Reader` →
adapter*. An adapter cannot shortcut into a model mutation; that is enforced by *who
holds the writer*, not by review.

The model owns **all domain state M4 will need**, so M4 doesn't rediscover edge
cases later:

- **Live-snapshot handling lives here from M0.** `append_log` reproduces the TUI
  reducer's exact `LogUpdateKind` logic — `Append`, `ReplaceLast`, and
  `LiveSnapshot { id }` (append-or-replace by id) — and the model owns
  `live_snapshot_id`, resetting it on `mark_started` and `ClearLogs`. Otherwise M4
  would re-implement these interactive-output edge cases.
- **Bounded healthcheck history**, not just the last attempt (the TUI shows
  history). `get_health` returns the latest; M4 reads the whole ring.
- **Public types are deliberate.** Export stable `Desired`, `Execution`, `Health`,
  `RestartPolicy` from the crate root for the model API; the control crate reuses
  those stable payloads inside its protocol envelopes. The public model surface
  must not leak private/internal modules.

**Why `SessionChange`, not the full `Event`, on the broadcast:** the broadcast is
liveness-only. `broadcast` drops for lagging receivers, so it must not be the
carrier of content (log strings, etc.). Subscribers receive a tiny
`{ service_id, kind }` and **re-query the model**, which holds the content.
A lagging subscriber loses nothing but a coalescible notification.

**What "lossless" means here (corrected).** Logs are appended to the model in the
scheduler's own task, so they are **lossless from the scheduler onward** — the
model reflects everything the scheduler received, regardless of TUI backpressure.
But the PTY-reader→scheduler hop is already a bounded `try_send` (fact #3), so
end-to-end logs remain **best-effort by design**. The model is authoritative over
"everything the scheduler saw," not "every byte the child wrote." We document this
rather than pretend otherwise.

**Invariant: the scheduler never awaits a frontend.** Today non-log events use
`ui_tx.send(..).await`, so a wedged-but-open TUI channel could pause the scheduler —
which would mean an adapter *can* affect the core. M0 uses a small **legacy TUI
bridge task**: the scheduler writes the model, then non-blockingly hands legacy
events to the bridge; the bridge owns any await on the old TUI channel. The drop
policy is **not uniform**, because the pre-M4 reducer still depends on ordered
lifecycle events:

- **While the bridge is healthy:** log frames (`LogLine`/live snapshots) may be
  dropped or coalesced (already best-effort, fact #3), but lifecycle/status events
  (`Started`, `Exited`, `Killed`, `Disabled`, `ClearLogs`, health transitions) are
  delivered **in order and not silently coalesced** — the pre-M4 reducer depends on
  them.
- **If the lifecycle bridge overflows (the TUI is wedged):** the old reducer is
  treated as **degraded** — explicitly, by detaching that frontend (a frozen UI,
  not a silently-corrupted one) rather than dropping lifecycle events mid-stream or
  growing unbounded. The honest claim is *not* "the old reducer is always perfect
  under backpressure"; it is "**the model stays authoritative**, the scheduler never
  blocks, and a degraded frontend is visibly degraded." After M4 the TUI reads
  `SessionModelReader` + `SessionChange` directly and the path disappears.

This makes "adapters cannot stall the core" honest without pretending the legacy
reducer is perfect under sustained backpressure.

**Core API — capability handles only.** The TUI is trusted, in-process, and keeps
the full `mpsc::Sender<Command>` capability it already needs today for
`SendInput`/`ResizeAll`. The untrusted boundary is the control/MCP adapter
constructor, which receives a narrow `ServiceControl` wrapper exposing only safe
service operations:

```rust
// Restricted capability — the ONLY command port handed to untrusted adapters
// (control server, MCP). Each method builds a safe Command variant internally (no
// SendInput/ResizeAll method exists, so they are unreachable) and is **request/response**:
// the command carries a oneshot reply, so the *scheduler* validates and latches the
// generation at the exact moment it processes the command — no pre-queue snapshot race.
#[derive(Clone)]
pub struct ServiceControl { tx: mpsc::Sender<Command> }    // restart, restart_all, enable, disable
pub enum CommandRejection { UnknownService, InvalidState }  // e.g. restart on a Disabled service
type ServiceCommandResult = Result<Vec<ServiceCommandAck>, CommandRejection>; // Vec covers RestartAll
impl ServiceControl {
    pub async fn restart(&self, id: &ServiceID) -> Result<ServiceCommandResult, SchedulerStopped>;
    pub async fn restart_all(&self)             -> Result<ServiceCommandResult, SchedulerStopped>;
    pub async fn enable(&self, id: &ServiceID)  -> Result<ServiceCommandResult, SchedulerStopped>;
    pub async fn disable(&self, id: &ServiceID) -> Result<ServiceCommandResult, SchedulerStopped>;
}
// Two nested Results: the OUTER Err is transport (the scheduler dropped the reply → SchedulerStopped);
// the INNER Err is the scheduler's typed rejection; Ok(Ok(acks)) carries each affected service's
// latched observed_generation (a Vec, so RestartAll fits). The control server maps
// Ok(Ok)→Accepted, Ok(Err)→Error{code}, Err(SchedulerStopped)→Error{SchedulerStopped}. The Command
// service-control variants gain an Option<oneshot::Sender<ServiceCommandResult>>; the TUI passes
// None (fire-and-forget, unchanged).

pub struct Handles {
    pub reader: SessionModelReader,        // READ: query + subscribe()
    pub commands: mpsc::Sender<Command>,   // full trusted in-process command sender for the TUI/CLI
}

impl Micromux {
    /// Non-async: builds the model (`Inner` + `Writer` kept by the scheduler) and the
    /// command channel internally, returns the `Reader` + command sender
    /// alongside the runner future. `Arc<Self>` makes the future `'static` so the caller
    /// can `tokio::spawn` it while holding the Handles. The `Writer` never leaves the core.
    pub fn start_with_handles(
        self: std::sync::Arc<Self>,
        ui_tx: mpsc::Sender<Event>,        // transitional: feeds the unchanged TUI until M4
        shutdown: CancellationToken,
    ) -> (impl std::future::Future<Output = eyre::Result<()>> + 'static, Handles);
}
```

The enforcement point is **type, not discipline**: `ControlServer::new` and the MCP
adapter take `ServiceControl`, which has no `send_input`/`resize_all` method and
cannot construct those variants — so input forwarding is *unreachable* for them.
The TUI keeps the full sender because it is the trusted in-process frontend that
already owns PTY input and resize today. `micromux` is still pre-1.0; once the CLI
migrates to `start_with_handles`, prefer changing/removing the old `start(...)`
signature instead of carrying a compatibility shim for a hypothetical external
consumer.

**Tests:** projection unit tests (each `ServiceRuntime` transition →
expected snapshot: desired vs execution, run_generation bump on restart, exit
code, uptime anchor); **a wedged TUI cannot stall the scheduler** — fill `ui_tx` to
capacity and assert the scheduler keeps **processing further commands and
transitions** (not merely that the model saw the first one), while TUI frames may
drop; a `SessionChange`/re-query round-trip.

**Acceptance:** behavior is identical for a responsive TUI; under a wedged TUI the
bridge may drop/coalesce legacy visual frames instead of stalling supervision.
`cargo test` green; model reflects scheduler truth under load.

### M1 — Control plane: the per-session control endpoint

Add a second adapter in `micromux-cli` driven off the M0 handles. **Default on,
opt-out two ways (decided):** a CLI flag (`--no-control`) and a config-file
setting (top-level `control: { enabled: false }`). Also auto-disabled if no
runtime dir is resolvable. The CLI flag wins over the config setting.

Define the **endpoint enum now** (even though only Unix lands first), so Windows is
not a retrofit without adding speculative trait/generic machinery:

```rust
enum ControlEndpoint { Unix(PathBuf), WindowsNamedPipe(String) }
// micromux-control exposes concrete bind/accept/connect functions that match on
// this closed set. The core knows none of it.
```

On startup (the session is the *only* writer on disk — the proxy never writes):
1. Resolve the **runtime dir** (see spec) and ensure `…/micromux/` exists with
   platform-appropriate perms.
2. Compute the endpoint deterministically from the canonical config path:
   `…/micromux/<hash>.sock` (Unix) / `\\.\pipe\micromux-<hash>` (Windows). Bind it
   via the **race-safe dance** (see spec: lifetime-held lock),
   so concurrent same-config starts and crash-leaked sockets are handled without
   ever unlinking a live peer's socket.
3. Spawn an accept loop (one task per connection). The `ControlServer` is
   constructed with exactly two capabilities — a `SessionModelReader` (queries +
   `subscribe()`) and a `ServiceControl` (mutations). It holds **no writer** (so
   it cannot mutate the model — a command becomes a write only after the scheduler
   processes it) and **no input port** (`SendInput`/`ResizeAll` are not expressible
   through `ServiceControl`). `Describe` returns session identity (pid,
   start_time, name, working_dir, config_path, services, protocol version).

On shutdown (hook the existing `CancellationToken`): unlink the socket while still
holding the lifetime lock. A crash that skips this leaves an inert socket that the
next same-project start reclaims after acquiring the lock — no background reaper.

Optional in this milestone: `micromux ctl {ls|logs|restart|…}` — a tiny client in
the same binary (not feature-gated). Exercises the protocol end-to-end with no
MCP/agent in the loop and gives humans/scripts a CLI.

**Tests:** boot the core against a temp config, connect, assert
`list_services` / `restart` / `get_logs` / `Describe`; **concurrent same-config
startup** (two cores race the same hash → exactly one acquires the lifetime lock
and binds; the other runs with control disabled, no second endpoint); leaked
socket reclaim after crash/forced exit.

**Acceptance:** with micromux running, `micromux ctl` lists services, tails logs,
restarts a service; the socket is cleaned up on exit and a leaked one is reclaimed
on the next same-project start.

### M2 — MCP server (`micromux mcp`)

New `micromux-mcp` lib + `micromux mcp` subcommand. Use the official **`rmcp`**
Rust MCP SDK for stdio/JSON-RPC plumbing. Stateless: connect to a session endpoint
per tool call (cheap), hold no supervision state.

**All MCP code is feature-gated behind a default-on `mcp` feature** and lives in
one isolated module gated at the top (`#[cfg(feature = "mcp")] mod mcp;` in
`micromux-cli`, backed by the optional `micromux-mcp` dep). The clap `Mcp`
subcommand variant and its dispatch arm are `#[cfg(feature = "mcp")]` too, so with
the feature off the subcommand, the module, and `rmcp` all vanish at compile time.
The control plane (M0/M1) is *not* gated — it has no `rmcp` dependency and is
useful on its own (e.g. `micromux ctl`).

Session selection uses a typed selector, not an overloaded string (see spec).
Tools (v1):

| Tool | Args | Returns | Backed by |
|---|---|---|---|
| `list_sessions` | — | id, name, cwd, **config_path**, pid, services | endpoint scan + `Describe` |
| `list_services` | `session?` | **resolved session config_path**; per service: name, desired, execution, health, ports, uptime, restart policy, last exit, run_generation | `SessionModelReader::services` |
| `get_logs` | `service`, `session?`, `tail?` (default + capped) | recent log lines | `SessionModelReader::logs` |
| `restart_service` | `service`, `session?` | `Accepted` → `G` (gen *before* restart); `InvalidState` if disabled | `Command::Restart` |
| `restart_all` | `session?` | `Accepted` (enabled services only; disabled skipped) | `Command::RestartAll` |
| `enable_service` | `service`, `session?` | `Accepted` → `G` (gen *before* enable) | `Command::Enable` |
| `disable_service` | `service`, `session?` | `Accepted` (gen informational; no healthy wait implied) | `Command::Disable` |
| `get_health` | `service`, `session?` | latest probe: success, exit code, command, output | HC history (latest) |
| `wait_for_healthy` | `service`, `after_generation?`, `timeout`, `session?` | healthy / exited(code) / timeout / typed error | see below |

Mutations are **`Accepted`, not done**, and **validation + the generation come from
the scheduler, not a pre-queue reader snapshot** — otherwise an auto-restart or a
prior queued command could advance the generation between a server-side snapshot and
the scheduler actually processing the command, and `wait_for_healthy(after_generation
= G)` would key off the wrong run. So the `ServiceControl` call is request/response:
the scheduler **validates and latches `observed_generation` at the exact moment it
processes the command** and replies. A closed reply (scheduler shutting down) →
**`SchedulerStopped`**, never a spurious `Accepted`. Per-command semantics:
- `restart_service` / `enable_service`: the latched generation *before* the action,
  used as `G` in `wait_for_healthy`.
- **`restart_service` on a `Disabled` service → `InvalidState`** (not a silent
  re-enable): `enable_service` is the operation that starts a disabled service, so
  `desired == Disabled` keeps its meaning. **Milestone boundary (explicit):** the
  scheduler enforces this strict rule only for **acknowledged commands** (those
  carrying a reply — i.e. `ServiceControl`, from M2). The **TUI's fire-and-forget
  restart key keeps today's behavior unchanged through M0–M3** (it has no reply
  channel to receive a rejection on), so M0's "no behavior change" claim holds; the
  two paths are unified at M4 when the TUI moves onto the model.
- `disable_service`: generation informational (no healthy wait implied).
- `restart_all`: acks only the enabled services it actually restarted.

`get_logs` is **bounded independently of the request frame**: `tail: None` could
otherwise exceed the 1 MiB frame cap. Apply a default tail (e.g. 200 lines), a max
tail, and a `max_bytes` response cap (drop oldest beyond it). Large histories are
paged by the caller, not returned whole. **`get_health` is capped the same way** —
even though the model already stores bounded HC history, the *response* applies its
own max lines / `max_bytes` so a chatty probe can't blow the frame.

**`wait_for_healthy` — the other half of the control loop** (here in M2, not M3:
`restart_service` without it is only half a loop). It is **generation-aware** to
avoid the restart race: an agent that calls `restart_service` (returns `G`) then
`wait_for_healthy(after_generation = G)` must not observe the *pre-restart* Healthy
state.

- With `after_generation = G`: resolve when, for a run with `run_generation > G`,
  `execution == Running && (healthcheck_configured ? health == Healthy : true)`.
- **`after_generation` omitted**: accept the *current* state (no new run required) —
  this is the "is it healthy right now?" query.
- **`run_generation == 0`** means never started; the wait then blocks for the first
  run to come up (or times out / fails fast per below).
- **Fails fast with `InvalidState`** (not a timeout) if `desired == Disabled` and no
  generation past `G` is in flight — a disabled service will never become healthy.
- Fails on `Exited` (returns the exit code) or `timeout`.
- **Race-free**: subscribe → query snapshot → wait on changes (re-query each; treat
  `broadcast::RecvError::Lagged` as "re-query now"), so a transition between the
  read and the subscription can't strand the wait. Not a fixed-interval poll.

**Tests:** in-process core + endpoint, call tool handlers, assert each behavior; a
discovery test with two fake endpoints (stub listeners) asserting cwd-derived
selection, explicit selector override, and that a refusing (dead) endpoint is
skipped.

**Acceptance:** `micromux mcp` in Claude Code lists/controls a running session
with zero selector args when launched in that project's dir.

### M3 — Ergonomics & polish

- **Optional config `name:`** — top-level identifier surfaced as the session id;
  add to the v1 parser (`config/v1.rs`) and known top-level keys. Falls back to
  `basename(working_dir)`, disambiguated by pid.
- **Optional log streaming** — a `follow_logs` tool. It must **not** rely on
  `SessionChange` ordering for log content: that broadcast is a coalescible liveness
  signal (re-query is correct for *snapshots*, lossy for a byte stream). Streaming
  uses an **explicit cursor / monotonic log-entry id** so a follower resumes exactly
  where it left off without gaps or dupes.
- Docs: README section + agent config snippets (Claude Code + Codex) for the
  `~/dev/configuration` repo.

### M4 — TUI consolidation onto the model (required, not optional)

Make "one lifecycle model, many adapters" real by deleting the duplicate. The TUI
stops reducing the event stream into its own *domain* state and instead reads
`SessionModelReader` snapshots and subscribes to `SessionChange`. **This is not a visual
rewrite** — `render.rs` and the look stay; only the domain-state plumbing moves.

State split:

- **TUI keeps (view state):** selected service, sidebar width, scroll offsets,
  cached rendered/wrapped text, dirty flags, follow-tail / wrap toggles.
- **Model owns (domain state), removed from `micromux-tui`:** execution state,
  desired state, health, logs + live-snapshot handling, healthcheck attempts,
  ports, restart policy, last exit, uptime, run_generation.

After M4 the duplicated `reducer::apply` domain logic is gone; the TUI computes
view state from model snapshots, using `SessionChange` to know what to refresh
(log appends can re-read the bounded tail, or carry a small append delta, to avoid
full re-render). The transitional `ui_tx`/granular `Event` path can then be retired
or kept only where a view genuinely needs streaming deltas.

**Ordering note:** the model is scheduler-authoritative from M0, so MCP
correctness does **not** *depend* on M4 — but the **recommended build order runs
M4 right after M1**, before the agent adapters (M2/M3). Folding the TUI — the most
demanding consumer — onto the model is the best proof the model is *complete*
before anything is built on it, and it collapses the duplicate-model window to a
minimum. Shipping M2 before M4 is acceptable if the agent loop is urgent, but it
knowingly carries duplicate domain state longer. Either way M4 is a committed
milestone, not a someday cleanup (two materialized models is a standing drift
hazard). See Suggested sequencing.

**Tests:** TUI renders identical frames from the model for the existing scenarios
(reuse the `micromux-screenshot` scenarios / `reducer.rs` cases as fixtures);
no domain reducer remains in `micromux-tui`.

---

## Detailed specs

### Runtime dir resolution & permissions

- **Linux:** `directories::ProjectDirs::runtime_dir()` → `$XDG_RUNTIME_DIR/micromux/`
  (already on `directories = "6"`).
- **macOS / Windows:** `runtime_dir()` is `None`. Fall back to a per-user dir
  (`std::env::temp_dir()/micromux-<uid>/`).
- If none resolvable — or the platform's transport is not yet implemented (e.g.
  Windows before **M1-Windows**) — warn and run with the control plane **disabled**
  (TUI still works). The binary is never half-working: control is either fully
  available or cleanly absent.

Permissions are **platform-specific**, and for Unix sockets the **directory** mode
is what actually gates access (a peer must traverse the dir to `connect`):

- **Unix:** directory mode `0700`; set the socket `0600` too, defensively.
- **Windows:** secure the named pipe with an ACL restricting it to the current
  user's SID (there is no `chmod`); the sentinel dir uses a current-user ACL.

### Endpoint layout & the `Describe` handshake

The endpoint name is **deterministic from the project**: `<hash>` is a short
collision-resistant digest of the *canonical config path* (the same config
micromux's `find_config_file` resolves). Session and proxy derive the identical
name from the same input, so the common one-session-per-project case needs **no
enumeration** — the proxy computes the name and connects. A concurrent second
instance on the same config does **not** create a second endpoint — it runs with
control disabled (see the dance spec), so there is exactly one endpoint per project.

**macOS socket-path length:** AF_UNIX `sun_path` is short (~104 bytes on macOS vs
~108 on Linux). Keep the runtime root compact and the hash fixed-length so
`<root>/micromux/<hash>.sock` stays well under the limit; if a resolved path would
still exceed it, fall back to a shorter root (or error with guidance) rather than
silently truncate.

All session identity/metadata is returned *live* by `Describe`, never stored in a
file:

```
Describe → { protocol_version, pid, start_time, name, working_dir,
             config_path, services: [..], micromux_version }
```

`pid` + `start_time` form a **start token** that defends against PID reuse for
Windows sentinel records. `name` is the config `name:` (M3) else
`basename(working_dir)`.

The supported transports are a **closed set** — no network transport, ever (so no
TCP, no per-session auth token, no browser-style local-trust problem):

- **Unix/macOS — Unix domain socket; no metadata registry.** Discovery uses sockets
  only (`readdir` `*.sock` + connect + `Describe`); there is no metadata file to
  drift, race, or leak. The only other file is the permanent per-hash `.lock`, which
  exists solely for ownership coordination and is never read for discovery.
- **Windows — named pipe with a current-user ACL + a sentinel *directory*.** Named
  pipes are not filesystem-enumerable, so each session writes **one sentinel file per
  hash** (`…/micromux/<hash>.json`, carrying pipe name + start token) — mirroring the
  one-socket-per-hash Unix layout, **not** a single global index. `list_sessions`
  `readdir`s the sentinel dir, reads each, and verifies it by connect + `Describe`.
  This sidesteps the cross-session write race a global index would have (two projects
  hold *different* per-hash locks, so they could atomically clobber a shared file):
  - each session writes/removes **only its own `<hash>` sentinel**, under its own
    per-hash lifetime lock — single-writer-per-file, no global lock, no compaction;
  - creation/update is an **atomic replace** (write temp → fsync as appropriate →
    rename) under the per-hash lock, so `list_sessions` never observes a half-written
    sentinel (no transient false negative);
  - the **proxy only skips** sentinels that fail to connect (read-only, never edits);
  - **malformed/partial sentinels are ignored** on read, never fatal (belt-and-braces
    behind the atomic replace);
  - a leaked sentinel (crash) is reclaimed when the next same-hash session takes the
    lock and rewrites it.
- **Any other platform — unsupported.** The control plane / MCP is cleanly absent
  (see the gating note below); the TUI still works.

If the Windows named-pipe + sentinel work feels disproportionate to ship first,
**gate the control plane off on Windows** initially (treat it like an unsupported
platform) — a cleaner trade than weakening the transport model with TCP.

**Unsupported must be explicit, not silent.** On a platform without a transport, do
not let `micromux mcp` look like "nothing is running." Prefer **compiling the `mcp`
/ control subcommands out** of unsupported builds, so `micromux mcp` is simply not a
command. If they are present, every tool returns a clear **`UnsupportedPlatform`**
diagnostic ("control plane is not supported on this platform"), distinct from an
empty session list. Same for the session-side: a control-disabled session advertises
nothing and logs why.

### The race-safe bind / reclaim dance

A naive "see stale socket → unlink → bind" has three failure modes: a TOCTOU race
(A decides to unlink; B binds; A unlinks B's live socket), a symmetric shutdown
race (an old process unlinks a successor's fresh socket), and a
**misclassification risk** — connect-probing a live-but-overloaded listener whose
backlog is full can look "refused." Close all three by making a **lifetime-held
ownership lock the authoritative ownership signal**, not connect-probing:

1. A session acquires an exclusive ownership lock for `<hash>` and **holds it for
   its entire lifetime**. On Unix/macOS this is a permanent
   `…/micromux/<hash>.lock` file locked with `flock` and never unlinked. On Windows
   M1-Windows uses the platform equivalent (named mutex or lock file compatible
   with the sentinel index). The OS releases it automatically on process exit,
   *including crash*, so "lock acquirable" ⇔ "no live owner" — more robust than
   connect-probing, which can misread a wedged listener.
2. **Acquired the lock** ⇒ no live owner: `unlink` any stale endpoint and bind.
   **Could not acquire** ⇒ a live owner holds this project; do **not** touch its
   endpoint (second-instance policy below).
3. On shutdown, while still holding the lifetime lock, unlink the endpoint. A
   successor cannot have bound the same path yet because it cannot acquire the lock
   until this process exits/releases it. Windows start tokens remain useful as
   sentinel identity, not as an ownership guard.

**Same-config second-instance policy (decided):** there is **at most one control
endpoint per project**, matching `Current` selection (which connects only
`<hash>`). A second micromux on the same config that cannot acquire the lock
**runs with control disabled and logs a warning** rather than creating a
`<hash>-<pid>` endpoint the cwd selector would never find. (A future
`--control-required` could make this a hard error instead.) This removes the
pid-suffix variant entirely and keeps discovery unambiguous: one project, one
endpoint, one writer of it.

### Liveness = connectability, not latency (invariant)

**A session's liveness is decided by the kernel's connection result, never by how
fast it replies.**

- Only a **hard connection error** — `ECONNREFUSED` / `ENOENT` — means **dead** (a
  unix socket file outlives its process; an orphan refuses connections).
- A live-but-busy session usually still accepts at the kernel level (the backlog is
  in-kernel), so it connects while its loop is slow. But a **saturated backlog** can
  make `connect()` block or time out — that is **`Busy`/unknown, never `dead`**, and
  **never eligible for cleanup**. Cleanup keys only on the hard errors above.
- A *reply* timeout (a request already delivered) likewise returns `Busy` — it
  **never deletes or de-lists the session.**

Corollary: a laggy session can never be "healed away." Only connection-level
failure (refused / gone / start-token mismatch) marks a session absent. Note this
governs the **read path** (the proxy, which never mutates). The **write path** (a
session reclaiming a stale endpoint) uses the lifetime ownership lock as its
signal instead — robust even when connect would be ambiguous under backlog
pressure.

### Desired vs execution projection (table)

`disable()` sets the scheduler's `state` to `Disabled` while the process may still
be **running and draining** — exactly the sticky-state ambiguity the split is meant
to remove. The projection from `ServiceRuntime` (`desired`, `running`, internal
`state`, retained `last_*`) to `(desired, execution, health)` is therefore explicit:

| `desired` | `running` | internal state | → `execution` | notes |
|---|---|---|---|---|
| Enabled | None | Pending, never started (`last_run_id == None`) | **Pending** | waiting on deps / initial start |
| Enabled | Some | Starting | **Starting** | |
| Enabled | Some | Running{health} | **Running** | `health` carried separately |
| Enabled | Some | Killed (restart in flight) | **Stopping** | restart requested, draining |
| Enabled | None | Exited (`last_run_id == Some`) | **Exited** | incl. crash and backoff-before-restart; `last_exit_code` set |
| **Disabled** | **Some** | Disabled (cancel in flight) | **Stopping** | **draining — not Exited/Pending** |
| Disabled | None | Disabled, ran before | **Exited** | stopped by disable; `last_exit_code` set |
| Disabled | None | Disabled, never ran | **Pending** | disabled and idle |

`health` is `Some(_)` only while `execution == Running` and a probe has resolved;
otherwise `None`. The decisive row is **Disabled + running=Some → Stopping**: a
disabled service that is still draining is never reported as already-Exited.

### Control wire protocol (`micromux-control`)

Newline-delimited JSON, request/response, with a **max frame size** (e.g. 1 MiB —
oversized frames are rejected, not buffered) and **per-request + idle timeouts**
(a broken client cannot pin memory or a task forever). `serde`-tagged enums:

```rust
enum Request {
    Describe,
    ListServices,
    GetLogs { service: ServiceID, tail: Option<usize> },
    GetHealth { service: ServiceID },
    Restart { service: ServiceID },
    RestartAll,
    Enable { service: ServiceID },
    Disable { service: ServiceID },
    Subscribe,                          // streams SessionChange until the client disconnects
}
enum Response {
    Description(SessionInfo),
    Services(Vec<ServiceSnapshot>),
    Logs { lines: Vec<LogLine> },
    Health(Option<HealthAttempt>),
    Accepted { services: Vec<ServiceCommandAck> }, // queued (validated) — NOT "completed"
    Change(SessionChange),              // only after Subscribe
    Error { code: ErrorCode, message: String },
}
struct ServiceCommandAck { service: ServiceID, observed_generation: u64 }
enum ErrorCode { UnknownService, NoSession, Ambiguous, Busy, Timeout, InvalidState,
                 SchedulerStopped, UnsupportedPlatform, ProtocolVersionMismatch, BadRequest, Internal }
```

`Accepted` carries a list so it fits `RestartAll` (every affected service) as well
as single-service mutations and enable/disable. The MCP `restart_service` tool
flattens the single ack and surfaces its `observed_generation` as `G` for
`wait_for_healthy(after_generation = G)`.

`Describe` carries `protocol_version`; a mismatch yields a hard
`ProtocolVersionMismatch` so an old proxy against a new session (or vice versa)
fails loudly, not weirdly. **Compatibility expectation:** the session and the proxy
are expected to be the *same installed binary version* (they are literally the same
binary, `micromux mcp` vs `micromux`); there is **no cross-version compatibility
guarantee pre-1.0**, and a mismatch is a hard error, not a negotiation. Protocol
envelopes (`Request`, `Response`, `Describe`) live in `micromux-control`; domain
payloads (`ServiceSnapshot`, `HealthAttempt`, `LogLine`, etc.) are stable core
types that derive serde and are reused directly. If compatibility pressure appears
later, DTO mirrors can be introduced then; v1 should not duplicate payload structs
for a same-version internal protocol.

### Session selection (MCP server) — read-only, connect-to-verify, typed selector

```rust
enum SessionSelector { Current, Name(String), Pid(u32), ConfigHash(String) } // tools take Option<…>, default Current
```

The proxy never mutates the filesystem; it only connects.

1. Explicit selector (`Name`/`Pid`/`ConfigHash`) → resolve to its endpoint (scan +
   `Describe` to match); error `NoSession` if none answers. **If a `Name` matches
   more than one live session, return `Ambiguous` — never silently pick one** (two
   projects could share a `name:`; picking arbitrarily could drive the wrong one).
2. Else `MICROMUX_SESSION` env → parsed as a selector.
3. Else `Current`: run micromux's own `find_config_file` upward from the proxy's
   cwd (the project root the client launched it in), canonicalize, hash, connect.
   Connects ⇒ that's the session; refused/absent ⇒ `NoSession` ("start micromux").
   **Zero enumeration on the happy path.**
4. `list_sessions` / disambiguation scan, connect, `Describe`, and silently skip
   the ones that refuse.

**Selection is ambient, so make the target legible.** cwd, `MICROMUX_SESSION`, or a
shared `name:` can all point at the wrong project. The endpoint is same-user only
(so this is a *wrong-target* risk, not a privilege one), but to keep the agent and
human honest, `Describe` always carries `config_path`, and `list_sessions` /
`list_services` **surface the resolved session's `config_path` prominently** so it
is obvious which micromux is being driven before any mutation.

If a session was started with `--config /elsewhere/micromux.yaml`, the cwd-derived
happy path will not find it unless the proxy is launched from a directory whose
config search resolves to the same canonical path. That is expected; use an
explicit selector or `list_sessions` for non-default config locations.

---

## Crate / module layout

```
crates/
  micromux/                 # core — knows nothing about sockets/pipes/MCP/JSON/discovery
    src/model.rs            # NEW: Inner (private) + SessionModelReader (pub, Clone) + possession-scoped SessionModelWriter; ServiceSnapshot; SessionChange; pure &ServiceRuntime->ServiceSnapshot projection; SessionModel::new()
    src/scheduler.rs        # private runtime state; transition methods mutate runtime + write_snapshot via the Writer; append_log on LogLine; handle_command validates + latches observed_generation, replies on the command's oneshot
    src/scheduler/types.rs  # internal RunId, surfaced publicly as run_generation; desired/execution projection
    src/lib.rs              # start_with_handles(self: Arc<Self>, ui_tx, shutdown) -> (future, Handles{reader, commands: mpsc::Sender<Command>}); ServiceControl wrapper for untrusted adapters
  micromux-control/         # NEW lib: wire envelopes + Describe; ControlEndpoint; concrete client/server framing; path derivation; dir resolution
  micromux-cli/
    Cargo.toml              # [features] default = ["mcp"]; mcp = ["dep:micromux-mcp"]
    src/control/mod.rs      # NEW: run ControlServer over a Reader + ServiceControl; race-safe lifetime-lock bind/reclaim
    src/control/ctl.rs      # OPTIONAL: `micromux ctl` client subcommand (not feature-gated)
    src/mcp.rs              # NEW: `#[cfg(feature = "mcp")]` thin shim → micromux-mcp; gated at the top
    src/options.rs          # control flags; `ctl` subcommand; `Mcp` variant under #[cfg(feature = "mcp")]
    src/main.rs             # wire adapters off start_with_handles; dispatch subcommands
  micromux-mcp/             # NEW lib (optional dep): rmcp tool adapter, discovery; no supervision state; driven by `micromux mcp`
  micromux-tui/             # M4: read SessionModelReader + SessionChange; delete domain reducer, keep view state
```

### New dependencies

- `micromux-mcp`: `rmcp` (official MCP SDK), `tokio`, `serde`/`serde_json`,
  `micromux-control`. Pulled in by `micromux-cli` as an **optional** dependency
  enabled by the default-on `mcp` feature (`mcp = ["dep:micromux-mcp"]`), so `rmcp`
  is built only when the feature is on.
- `micromux-control`: `serde`/`serde_json`, `tokio` (UnixListener/UnixStream;
  named pipes on Windows), `directories` (already used). A small lock dependency
  (`fs2`/`fd-lock` or platform-specific equivalent) for the lifetime ownership
  lock.
- `micromux` core: `tokio::sync::broadcast` (tokio already present).

Mind the workspace lints (`unwrap_used`, `expect_used`, `panic`, `indexing_slicing`
all denied) — protocol parsing and socket handling must be fully fallible. **Enable
`clippy::await_holding_lock`** to mechanically enforce "no `parking_lot` guard held
across `.await`" (see robustness).

---

## Lifecycle, security, robustness

- **Endpoint perms (platform-specific):** Unix dir `0700` (the dir gates `connect`)
  + socket `0600`; Windows named-pipe ACL to the current user. No network, no
  other-user access by construction.
- **Liveness invariant:** only **hard connection errors** (`ECONNREFUSED`/`ENOENT`)
  mark a session dead; **reply or connect timeouts (incl. backlog saturation) mark
  `Busy`/unknown and never trigger cleanup** (see spec). Lag never de-lists.
- **Race-safe ownership:** a session holds the per-hash ownership lock for its
  whole lifetime — the authoritative "is there a live owner" signal, auto-released
  on crash. A successor cannot bind until the lock is released, so shutdown cleanup
  can unlink under the lock without an inode/start-token guard. The reclaim path
  never relies on connect-probing (which a wedged listener can fool).
- **Read-only proxy:** the MCP proxy never writes or deletes on disk — it *skips*
  dead endpoints/sentinel files, never prunes them. Only sessions mutate (each
  writes its own Windows sentinel file), under its own lifetime lock, touching only
  its own endpoint — single-writer-per-file.
- **Cleanup:** unlink on graceful shutdown; no reaper task — a crash-leaked socket
  is inert and reclaimed by the next same-project start's dance.
- **No locks across `.await`:** the model's `Inner` uses `parking_lot::RwLock` (sync).
  Adapters snapshot/clone (or copy the log tail) **under the lock, drop it, then
  serialize and write to the socket** — never hold the guard across an await or
  JSON serialization. Enforced by `clippy::await_holding_lock`.
- **Logs are best-effort end-to-end** (fact #3): lossless from the scheduler into
  the model; the upstream PTY→scheduler hop and interactive snapshots are
  intentionally droppable. The model is authoritative over what the scheduler saw.
- **Reach is currently-open sessions only:** closing the TUI exits the process,
  stops its services (by design), and removes the endpoint. The agent acts on
  exactly the sessions a human has open now — a property of the ephemeral model,
  not a bug. Changing it is the daemon decision (see Alternatives).
- **Opt-out:** `--no-control` (CLI) or `control: { enabled: false }` (config).
- **Tool-surface discipline:** read/observe + restart are the entire surface; every
  tool is typed — no raw stdin/keystroke channel, no arbitrary exec.

---

## Testing strategy

- **Unit:** the pure projection `&ServiceRuntime -> ServiceSnapshot` (each
  transition → expected snapshot, incl. the desired/execution mapping table and
  persisted `run_generation` across exit), tested without a lock or writer; wire +
  `Describe` serde round-trip incl. version mismatch; endpoint-name derivation from
  a config path; selector resolution.
- **Integration (the hard parts):** boot the core against a temp config (pattern
  already used throughout `scheduler.rs` tests) and assert —
  - **concurrent same-config startup**: two cores race one hash → exactly one
    acquires the lifetime lock and binds; the other runs with control disabled; no
    live socket is ever unlinked;
  - **leaked socket reclaim**: a crashed process leaves a socket file; the next
    same-config session acquires the released lifetime lock, unlinks the stale
    endpoint, and binds;
  - **restart-then-wait**: `restart_service` (gen `G`) then
    `wait_for_healthy(after_generation = G)` does **not** return on the pre-restart
    Healthy — only on the new run; `G` is **latched by the scheduler at processing
    time**, so an auto-restart racing between enqueue and processing can't hand back
    a stale generation;
  - **restart on disabled** → `InvalidState` (not a silent re-enable);
  - **legacy TUI backpressure isolation**: a full/wedged `ui_tx` does not stop the
    scheduler from processing later commands/transitions; a responsive TUI still
    receives lifecycle events **in order**; a wedged frontend is **detached** rather
    than fed a gap in `Started`/`Exited` (no silent reducer desync);
  - **command acceptance failure**: a mutation against a closed command channel
    returns `SchedulerStopped`, never a spurious `Accepted`;
  - **Windows endpoint selection** (`cfg`-gated): `ControlEndpoint` picks the named
    pipe; a **per-hash sentinel file** round-trips; the proxy **skips** sentinels
    that fail to connect (read-only, never edits); a leaked sentinel is reclaimed by
    the next same-hash session — no global index, no compaction race;
  - **unsupported-platform path is explicit, not empty**: with control gated off
    (e.g. Windows pre-M1-Windows), `micromux mcp` is either **absent** or returns a
    clear **`UnsupportedPlatform`** diagnostic — *never* an empty session list that
    reads as "nothing running"; no tool path half-starts then fails late; the TUI
    runs normally.
- **MCP:** in-process core + endpoint, call tool handlers, assert outputs and
  cwd-derived discovery.
- **Manual:** `micromux mcp` in Claude Code against `examples/demo`.

---

## Alternatives considered (discovery / control transport)

- **Registry JSON file + self-healing reaper (earlier draft) — superseded** by the
  socket-only design. The separate metadata file was the brittle part:
  write-before-bind races, a metadata file that can outlive (or precede) its
  socket, and a reaper that could mis-fire on a merely-laggy session. Deleting the
  file and fetching identity via `Describe` removes the whole failure class.
- **mDNS / Bonjour — no.** Cross-*host* multicast discovery: wrong scope, would
  expose dev services on the LAN, needs avahi/bonjour. Solves a problem we don't
  have and adds ones we don't want.
- **CRDT — no (category error).** CRDTs converge concurrent conflicting writes to
  shared replicated state. Here each session owns its own state, the proxy is a
  read-only observer, and commands are point-to-point RPCs — nothing to converge.
- **D-Bus — closest to a real local "self-discovery bus," but no.** Linux-only
  (kills macOS/Windows), heavy `zbus` dependency, assumes a session bus often
  absent in containers/CI/headless. Poor fit for a cross-platform CLI.
- **Single daemon (Docker model) — deferred, not rejected.** More robust for
  discovery (one well-known socket, one source of truth), but it changes what
  micromux *is*: a background service to install/run (systemd), daemon↔CLI version
  skew, and an inverted lifecycle where services outlive the TUI. It also
  complicates the careful PTY/signal/env handling the in-process model gets right
  (the PTY master must live where it is rendered → fd-passing or no PTY rendering).
  Wrong trade for an ephemeral per-project tool today.
  **De-risk:** the per-session wire protocol is identical whether the proxy dials
  one endpoint or many, so this can become an *optional* daemon later (for
  detach/persistence) without a protocol change. Choosing sockets now does not
  foreclose it.

## Decisions (settled)

1. **MCP packaging:** `micromux mcp` subcommand, one binary, behind a default-on
   `mcp` Cargo feature; all MCP code in one feature-gated module so `rmcp` is absent
   from `--no-default-features` builds. ✅
2. **Control plane:** default on; opt-out via CLI flag (`--no-control`) *or* config
   setting (`control: { enabled: false }`); CLI flag wins. ✅
3. **No `send_input`:** the surface is fully typed — no raw stdin/keystroke
   forwarding, now **type-enforced** (untrusted adapters hold only
   `ServiceControl`, which cannot express `SendInput`/`ResizeAll`; see #8). ✅
4. **Model is scheduler-authoritative:** the core model is written by the scheduler
   (via `SessionModelWriter`) from its own transitions — one lifecycle model, not a
   second reducer re-deriving it. ✅
5. **Snapshot models desired vs execution separately**, plus `run_generation`, so
   control APIs are unambiguous and `wait_for_healthy` can be race-free. ✅
6. **TUI consolidation (M4) is required**, not optional — it removes the duplicate
   model. The TUI keeps view state; domain state moves to the model. ✅
7. **Closed transport set, no network ever** (`ControlEndpoint`): Unix/macOS = Unix
   domain socket; Windows = named pipe with a current-user ACL; any other platform =
   **unsupported** (control plane cleanly absent). **No TCP fallback, no auth tokens.**
   Windows named pipes + sentinel discovery are their **own milestone (M1-Windows)**;
   until it lands — or as a deliberate first-ship choice — Windows is gated like an
   unsupported platform (control-disabled, never half-working). ✅
8. **Capability-split model + ports (compiler-enforced).** The model is a private
   `Inner` behind two handles: a **possession-scoped** `SessionModelWriter` (no
   public constructor, `!Clone`, built only by `SessionModel::new()` and moved into
   the scheduler future — *not* a `pub(in …)` path, which can't name a sibling module)
   and a `Clone` `SessionModelReader` (read + `subscribe()`) given to every adapter.
   Adapters affect state only through a narrow **`ServiceControl`** — through
   which `SendInput`/`ResizeAll` are not even expressible, so "no raw input
   forwarding" is type-enforced, not policy (the trusted TUI keeps the full
   `mpsc::Sender<Command>`). Lifecycle changes go through transition methods that update
   runtime **and** model together (no direct `state =`), so "forgot to sync" is
   impossible, not just discouraged. The core depends on no transport/protocol
   crate; `micromux-control` owns the `ControlEndpoint` enum and concrete
   client/server framing, so the same protocol boundary serves Unix sockets and
   Windows pipes. ✅

## Open questions to confirm before coding

1. **Post-M4 event path** — once the TUI reads the model, fully retire the granular
   `Event`/`ui_tx` path, or keep a minimal streaming-delta channel for log-append
   rendering performance? Default: keep a minimal log-append delta, retire the rest.

---

## Suggested sequencing

Milestone IDs are by role (M0 model, M1 control, M2 MCP, M3 ergonomics, M4 TUI
consolidation). The **recommended build order interleaves M4 early**, because the
TUI is the most demanding consumer and folding it onto the model is the best proof
the model is *complete* before the agent adapters are built on it:

1. **M0** authoritative model written by the scheduler (private state + transition
   methods that write the model, persisted generation); capability-split
   `Reader`/`Writer` + narrow `ServiceControl` for untrusted adapters;
   `start_with_handles` + `Handles`; `SessionChange`. Responsive-TUI behavior is
   unchanged; a wedged legacy TUI cannot stall supervision.
2. **M1** `micromux-control` + endpoint adapter for **Unix/macOS** (deterministic
   name, lifetime-lock dance, `Describe`, concrete endpoint enum) (+ optional `ctl`).
3. **M4** fold the TUI onto the model; delete the duplicate domain reducer —
   **validates model completeness** against the hardest consumer.
4. **M1-Windows** named-pipe transport + sentinel index behind the M1 abstraction.
   **Gates advertising Windows control-plane support**, so it lands before M2 *if*
   Windows parity is required at release; until then the Windows binary runs with
   the control plane **auto-disabled** (TUI works normally — never half-working).
5. **M2** `micromux-mcp` + `micromux mcp` with the full tool set **including
   `wait_for_healthy`** (generation-aware) — restart without it is only half the
   control loop.
6. **M3** config `name`, docs/agent snippets, optional log streaming.

Shipping M2 before M4 is acceptable if the agent loop is urgent, but it knowingly
carries duplicate domain state longer — so the default is M4 first. M4 is required
either way to reach one model, many adapters.

**M2 is the MCP release boundary**, and it now includes `wait_for_healthy` precisely
because restart-then-wait is the core agent workflow — the feature is not "done"
without generation-aware waiting. M3 is pure ergonomics layered on a complete loop.
