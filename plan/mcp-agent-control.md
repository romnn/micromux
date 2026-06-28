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
  network. **The port-conflict problem disappears.** On Unix/macOS the **socket is
  the only on-disk artifact** — its name is derived deterministically from the
  project's config path and all metadata is fetched live via `Describe`, so there
  is no registry file to drift, race, or leak. (Windows named pipes are not
  filesystem-enumerable, so they need a small sentinel index — see spec; the
  "socket-only" invariant is Unix/macOS-specific.)

Rejected alternative — *each session is its own HTTP/SSE MCP server*: agent
config needs a stable URL; a per-session ephemeral port has none, reintroducing
the discovery + allocation problem the proxy already solves for free.

---

## Component breakdown

| Component | Crate | Responsibility |
|---|---|---|
| Authoritative session model | `micromux` (core) | Private `Inner`; possession-scoped `SessionModelWriter` + `Clone` `SessionModelReader`; split `ServiceControlSink`/`TuiCommandSink` ports; `SessionChange` notifications. The enabling refactor. |
| Control protocol + client/server | `micromux-control` (new) | Wire DTOs, `Describe`/discovery, `ControlEndpoint` + `ControlClient`/`ControlServer` framing, path derivation. Shared by server + MCP. |
| Control endpoint adapter | `micromux-cli` | Bind the endpoint (race-safe dance) and clean up on exit; run `ControlServer` over a `Reader` + `ServiceControlSink` (no writer, no input-forwarding). |
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

**No user-visible behavior change. TUI unchanged in this milestone.** This is the
structurally important part: the model becomes the single source of truth, and it
is **written by the scheduler from its own state transitions** — not a second
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
pub enum Execution { Pending, Starting, Running, Stopping, Exited } // Stopping = today's Killed (awaiting exit)

pub struct ServiceSnapshot {
    pub id: ServiceID,
    pub name: String,
    pub desired: Desired,             // requested state (Disabled is a *desire*, not an execution)
    pub execution: Execution,         // observed lifecycle
    pub health: Option<Health>,       // Healthy/Unhealthy, None until first probe resolves
    pub run_generation: u64,          // scheduler RunId; bumps on every (re)start
    pub open_ports: Vec<u16>,
    pub healthcheck_configured: bool,
    pub last_exit_code: Option<i32>,
    pub uptime: Option<Duration>,     // since the current run's Started
    pub restart_policy: RestartPolicy,
}

// Private shared state. Neither handle exposes the lock; both wrap the same Arc<Inner>.
struct Inner { /* RwLock per service: snapshot + BoundedLog + live_snapshot_id
                 + bounded HealthCheck history; broadcast::Sender<SessionChange>; global seq */ }

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
/// the ONLY way to obtain one is `channel()` below — which hands it straight into the
/// scheduler future. It never appears in `Handles`, so no adapter can hold one.
pub(crate) struct SessionModelWriter { inner: Arc<Inner> }
impl SessionModelWriter {
    pub(crate) fn write_snapshot(&self, snap: ServiceSnapshot);     // bumps seq, publishes Change{Status}
    pub(crate) fn append_log(&self, id: &ServiceID, update: LogUpdateKind, line: String);
    // healthcheck lifecycle mirrors the scheduler's events (Started/LogLine/Finished),
    // so ordering and the result update stay structurally explicit:
    pub(crate) fn start_health_attempt(&self, id: &ServiceID, attempt: u64, command: String);
    pub(crate) fn append_health_line(&self, id: &ServiceID, attempt: u64, stream: OutputStream, line: String);
    pub(crate) fn finish_health_attempt(&self, id: &ServiceID, attempt: u64, success: bool, exit_code: i32);
}

/// The ONLY constructor for the model — returns the paired handles. `start_with_handles`
/// moves the writer into the scheduler future and returns only the reader.
pub(crate) fn channel() -> (SessionModelReader, SessionModelWriter);

pub struct SessionChange { pub seq: u64, pub service_id: ServiceID, pub kind: ChangeKind } // Status | Logs | Health
```

**Capability flow.** The writer is unforgeable (no public constructor, `!Clone`) and
is moved into the scheduler future, never into `Handles` — so the reader is the only
model handle an adapter ever sees. The only path by which an adapter can affect state
is: *adapter → `ServiceControlSink` → scheduler → `Writer` → model → `Reader` →
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
  `RestartPolicy` from the crate root for the model API; the wire crate has its own
  `serde` DTO mirrors. The public model surface must not leak private/internal
  modules.

**Why `SessionChange`, not the full `Event`, on the broadcast:** the broadcast is
liveness-only. `broadcast` drops for lagging receivers, so it must not be the
carrier of content (log strings, etc.). Subscribers receive a tiny
`{ seq, service_id, kind }` and **re-query the model**, which holds the content.
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
which would mean an adapter *can* affect the core. Forbid it: every scheduler→TUI
send becomes non-blocking (`try_send`, like logs already are; or the legacy `ui_tx`
is isolated behind a small **bridge task** that owns the backpressure). The model is
written *before* the forward, so a dropped frame is only a transient visual glitch
the TUI self-corrects from on the next event — and is gone entirely after M4, when
the TUI consumes `SessionChange` (a non-blocking broadcast) instead of `ui_tx`. This
is the structural form of "adapters cannot stall the core."

**Core API — capability handles only, with the command port *also* split.** A
single `Command`-sending port would let any adapter send `SendInput`/`ResizeAll`,
so "no raw input forwarding" would be policy, not types. Split it:

```rust
// Restricted capability — the ONLY command port handed to untrusted adapters
// (control server, MCP). Each method constructs a safe Command variant internally;
// there is no method that emits SendInput or ResizeAll, so they are unreachable.
#[derive(Clone)]
pub struct ServiceControlSink { tx: mpsc::Sender<Command> }    // restart, restart_all, enable, disable

// Full capability — TUI only (attach-mode input, terminal resize).
pub struct TuiCommandSink { tx: mpsc::Sender<Command> }        // service control + send_input + resize_all
impl TuiCommandSink { pub fn service_control(&self) -> ServiceControlSink; /* downgrade */ }

pub struct Handles {
    pub reader: SessionModelReader,        // READ: query + subscribe()
    pub commands: ServiceControlSink,      // SEND (restricted): the safe default for adapters
}

impl Micromux {
    /// Non-async: builds the model (`Inner` + `Writer` kept by the scheduler) and the
    /// command channel internally, returns the `Reader` + restricted `ServiceControlSink`
    /// alongside the runner future. `Arc<Self>` makes the future `'static` so the caller
    /// can `tokio::spawn` it while holding the Handles. The `Writer` never leaves the core;
    /// the full `TuiCommandSink` is obtained only via the dedicated TUI wiring path.
    pub fn start_with_handles(
        self: std::sync::Arc<Self>,
        ui_tx: mpsc::Sender<Event>,        // transitional: feeds the unchanged TUI until M4
        shutdown: CancellationToken,
    ) -> (impl std::future::Future<Output = eyre::Result<()>> + 'static, Handles);
}
```

The enforcement point is **type, not discipline**: `ControlServer::new` and the MCP
adapter take a `ServiceControlSink`, which has no `send_input`/`resize_all` method
and cannot construct those variants — so input forwarding is *unreachable* for them,
even from trusted wiring. The TUI is the sole holder of `TuiCommandSink`. The
existing `start(ui_tx, commands_rx, shutdown)` stays as a thin compatibility wrapper
over the same internal runner (it just doesn't expose the reader).

**Tests:** projection unit tests (each `ServiceRuntime` transition →
expected snapshot: desired vs execution, run_generation bump on restart, exit
code, uptime anchor); **a wedged TUI cannot stall the scheduler** — fill `ui_tx` to
capacity and assert the scheduler keeps **processing further commands and
transitions** (not merely that the model saw the first one), while TUI frames may
drop; a `SessionChange`/re-query round-trip.

**Acceptance:** TUI behaves identically; `cargo test` green; model reflects
scheduler truth under load.

### M1 — Control plane: the per-session control endpoint

Add a second adapter in `micromux-cli` driven off the M0 handles. **Default on,
opt-out two ways (decided):** a CLI flag (`--no-control`) and a config-file
setting (top-level `control: { enabled: false }`). Also auto-disabled if no
runtime dir is resolvable. The CLI flag wins over the config setting.

Define the **transport abstraction now** (even though only Unix lands first), so
the seam exists and Windows is not a retrofit:

```rust
enum ControlEndpoint { Unix(PathBuf), WindowsNamedPipe(String) }
// micromux-control exposes transport-agnostic `ControlServer` (bind/accept) and
// `ControlClient` (connect) over this. Unix sockets, Windows named pipes, and any
// future daemon endpoint satisfy the same API. The core knows none of it.
```

On startup (the session is the *only* writer on disk — the proxy never writes):
1. Resolve the **runtime dir** (see spec) and ensure `…/micromux/` exists with
   platform-appropriate perms.
2. Compute the endpoint deterministically from the canonical config path:
   `…/micromux/<hash>.sock` (Unix) / `\\.\pipe\micromux-<hash>` (Windows). Bind it
   via the **race-safe dance** (see spec: lifetime-held `flock` + inode-ownership),
   so concurrent same-config starts and crash-leaked sockets are handled without
   ever unlinking a live peer's socket.
3. Spawn an accept loop (one task per connection). The `ControlServer` is
   constructed with exactly two capabilities — a `SessionModelReader` (queries +
   `subscribe()`) and a `ServiceControlSink` (mutations). It holds **no writer** (so
   it cannot mutate the model — a command becomes a write only after the scheduler
   processes it) and **no input port** (`SendInput`/`ResizeAll` are not expressible
   through `ServiceControlSink`). `Describe` returns session identity (pid,
   start_time, name, working_dir, config_path, services, protocol version).

On shutdown (hook the existing `CancellationToken`): unlink the socket **only if
it still points at the inode this process bound** (see spec). A crash that skips
this leaves an inert socket that the next same-project start reclaims via the
dance — no background reaper.

Optional in this milestone: `micromux ctl {ls|logs|restart|…}` — a tiny client in
the same binary (not feature-gated). Exercises the protocol end-to-end with no
MCP/agent in the loop and gives humans/scripts a CLI.

**Tests:** boot the core against a temp config, connect, assert
`list_services` / `restart` / `get_logs` / `Describe`; **concurrent same-config
startup** (two cores race the same hash → exactly one acquires the lifetime lock
and binds; the other runs with control disabled, no second endpoint); **shutdown
unlinks only its own socket** (A leaks, B rebinds the path, A's shutdown must not
remove B's socket).

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
| `list_sessions` | — | id, name, cwd, pid, services | endpoint scan + `Describe` |
| `list_services` | `session?` | name, desired, execution, health, ports, uptime, restart policy, last exit, run_generation | `SessionModelReader::services` |
| `get_logs` | `service`, `session?`, `tail?` (default + capped) | recent log lines | `SessionModelReader::logs` |
| `restart_service` | `service`, `session?` | `Accepted` → `G` | `Command::Restart` |
| `restart_all` | `session?` | `Accepted` (all **enabled** services; disabled skipped) | `Command::RestartAll` |
| `enable_service` | `service`, `session?` | `Accepted` | `Command::Enable` |
| `disable_service` | `service`, `session?` | `Accepted` | `Command::Disable` |
| `get_health` | `service`, `session?` | latest probe: success, exit code, command, output | HC history (latest) |

Mutations are **`Accepted`, not done**: the server validates the service(s) exist,
forwards the command, and returns each affected service's *observed* generation.
"Accepted" means queued, not that the service restarted (see `wait_for_healthy`).

`get_logs` is **bounded independently of the request frame**: `tail: None` could
otherwise exceed the 1 MiB frame cap. Apply a default tail (e.g. 200 lines), a max
tail, and a `max_bytes` response cap (drop oldest beyond it). Large histories are
paged by the caller, not returned whole.

**Tests:** in-process core + endpoint, call tool handlers, assert each behavior; a
discovery test with two fake endpoints (stub listeners) asserting cwd-derived
selection, explicit selector override, and that a refusing (dead) endpoint is
skipped.

**Acceptance:** `micromux mcp` in Claude Code lists/controls a running session
with zero selector args when launched in that project's dir.

### M3 — Ergonomics & polish

- **`wait_for_healthy(service, after_generation?, timeout, session?)`** — the
  highest-value tool, and **generation-aware** to avoid the restart race: an agent
  that calls `restart_service` (returns generation `G`) then
  `wait_for_healthy(after_generation = G)` must not observe the *pre-restart*
  Healthy state. Resolves when, for a run with `run_generation > G`:
  `execution == Running && (healthcheck_configured ? health == Healthy : true)`.
  Fails on `Exited` (returns the exit code) or timeout. **Fails fast with a typed
  state error** (not a timeout) if `desired == Disabled` and no generation past `G`
  is in flight — a disabled service will never become healthy, so that should be an
  immediate `UnknownService`/disabled-style error, not a `timeout`. Implemented with the
  **race-free subscribe sequence** so a transition between the first read and the
  subscription can't strand the wait until timeout: **subscribe first, then query
  the snapshot, then wait on changes** (re-querying on each), and treat
  `broadcast::RecvError::Lagged` as "re-query now," not an error. Not a
  fixed-interval poll.
- **Optional config `name:`** — top-level identifier surfaced as the session id;
  add to the v1 parser (`config/v1.rs`) and known top-level keys. Falls back to
  `basename(working_dir)`, disambiguated by pid.
- **Optional log streaming** — a `follow_logs` tool over the existing `Subscribe`
  stream, once there's a need.
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
  user's SID (there is no `chmod`); the sentinel-index dir uses a current-user ACL.

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

`pid` + `start_time` form a **start token** that defends against PID reuse (for the
inode/start-token ownership checks and Windows sentinel records). `name` is the
config `name:` (M3) else `basename(working_dir)`.

- **Unix/macOS — socket-only invariant.** The socket is the only on-disk artifact;
  there is no metadata file to drift, race, or leak. Enumeration = `readdir` the
  dir for `*.sock` + connect + `Describe`.
- **Windows — named pipes + sentinel index.** Named pipes are not
  filesystem-enumerable, so `list_sessions` reads a small **sentinel/index file**
  (one record per session, carrying the start token) and verifies each record by
  connect + `Describe`. To stay consistent with the read-only proxy, the **proxy
  only *skips* records that fail to connect** — it never edits the file; the
  sentinel is written/compacted solely by session startup/shutdown under the
  per-hash lock. **Loopback TCP is a last resort only**, and only with a random
  per-session auth token and a 127.0.0.1 bind — otherwise it violates the
  no-network-exposure goal.

### The race-safe bind / reclaim dance

A naive "see stale socket → unlink → bind" has three failure modes: a TOCTOU race
(A decides to unlink; B binds; A unlinks B's live socket), a symmetric shutdown
race (an old process unlinks a successor's fresh socket), and a
**misclassification risk** — connect-probing a live-but-overloaded listener whose
backlog is full can look "refused." Close all three by making a **lifetime-held
advisory lock the authoritative ownership signal**, not connect-probing:

1. A session acquires an exclusive advisory **`flock` on `…/micromux/<hash>.lock`**
   and **holds it for its entire lifetime**. The kernel releases it automatically
   on process exit, *including crash*, so "lock acquirable" ⇔ "no live owner" —
   more robust than connect-probing, which can misread a wedged listener. (Runtime
   dirs are local — tmpfs / `$XDG_RUNTIME_DIR` — so advisory `flock` is reliable.)
2. **Acquired the lock** ⇒ no live owner: `unlink` any stale endpoint and bind.
   **Could not acquire** ⇒ a live owner holds this project; do **not** touch its
   endpoint (second-instance policy below).
3. After binding, record the endpoint's `(st_dev, st_ino)` (Unix) / start token
   (Windows). On shutdown, while still holding the lifetime lock, `stat` and
   **unlink only if it still matches** what this process bound — never a successor's.

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

- `connect()` → `ECONNREFUSED` / `ENOENT` ⇒ unambiguously dead. (A unix socket
  file outlives its process; an orphan refuses connections.)
- A session alive but *busy* still accepts at the kernel level (the listen backlog
  is in-kernel), so it connects fine even while its loop is slow.
- A timeout governs only "how long I wait for a *reply* to a request I already
  delivered." Hitting it returns a `Busy` error to the agent — it **never deletes
  or de-lists the session.**

Corollary: a laggy session can never be "healed away." Only connection-level
failure (refused / gone / start-token mismatch) marks a session absent. Note this
governs the **read path** (the proxy, which never mutates). The **write path** (a
session reclaiming a stale endpoint) uses the lifetime `flock` as its ownership
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
    Accepted { seq: u64, services: Vec<ServiceCommandAck> }, // queued (validated) — NOT "completed"
    Change(SessionChange),              // only after Subscribe
    Error { code: ErrorCode, message: String },
}
struct ServiceCommandAck { service: ServiceID, observed_generation: u64 }
enum ErrorCode { UnknownService, NoSession, Busy, Timeout, ProtocolVersionMismatch, BadRequest, Internal }
```

`Accepted` carries a list so it fits `RestartAll` (every affected service) as well
as single-service mutations and enable/disable. The MCP `restart_service` tool
flattens the single ack and surfaces its `observed_generation` as `G` for
`wait_for_healthy(after_generation = G)`.

`Describe` carries `protocol_version`; a mismatch yields `ProtocolVersionMismatch`
so an old proxy against a new session (or vice versa) fails loudly, not weirdly.
Snapshot/health/log DTOs are plain `serde::Serialize` mirrors of core types so the
core does not depend on the wire crate.

### Session selection (MCP server) — read-only, connect-to-verify, typed selector

```rust
enum SessionSelector { Current, Name(String), Pid(u32), ConfigHash(String) } // tools take Option<…>, default Current
```

The proxy never mutates the filesystem; it only connects.

1. Explicit selector (`Name`/`Pid`/`ConfigHash`) → resolve to its endpoint (scan +
   `Describe` to match); error `NoSession` if it does not answer.
2. Else `MICROMUX_SESSION` env → parsed as a selector.
3. Else `Current`: run micromux's own `find_config_file` upward from the proxy's
   cwd (the project root the client launched it in), canonicalize, hash, connect.
   Connects ⇒ that's the session; refused/absent ⇒ `NoSession` ("start micromux").
   **Zero enumeration on the happy path.**
4. `list_sessions` / disambiguation scan, connect, `Describe`, and silently skip
   the ones that refuse.

---

## Crate / module layout

```
crates/
  micromux/                 # core — knows nothing about sockets/pipes/MCP/JSON/discovery
    src/model.rs            # NEW: Inner (private) + SessionModelReader (pub, Clone) + SessionModelWriter (pub(in scheduler)); ServiceSnapshot; SessionChange; pure &ServiceRuntime->ServiceSnapshot projection
    src/scheduler.rs        # private runtime state; transition methods mutate runtime + write_snapshot via the Writer; append_log on LogLine
    src/scheduler/types.rs  # surface RunId as run_generation; desired/execution projection
    src/lib.rs              # start_with_handles(self: Arc<Self>, ui_tx, shutdown) -> (future, Handles{reader, commands: ServiceControlSink}); ServiceControlSink/TuiCommandSink; model::channel()
  micromux-control/         # NEW lib: wire DTOs + Describe; ControlEndpoint; ControlClient/ControlServer framing; path derivation; dir resolution
  micromux-cli/
    Cargo.toml              # [features] default = ["mcp"]; mcp = ["dep:micromux-mcp"]
    src/control/mod.rs      # NEW: run ControlServer over a Reader + ServiceControlSink; race-safe bind/reclaim; inode-guarded unlink
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
  named pipes on Windows), `directories` (already used). A small advisory-lock dep
  (`fs2`/`fd-lock`) for the bind `flock`.
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
- **Liveness invariant:** a session is alive iff its endpoint connects; lag never
  de-lists it (see spec). Reaping keys on connection failure, not reply latency.
- **Race-safe ownership:** a session holds the per-hash `flock` for its whole
  lifetime — the authoritative "is there a live owner" signal, auto-released on
  crash. Unlink is additionally inode (Unix) / start-token (Windows) guarded so a
  process never removes a successor's endpoint. The reclaim path never relies on
  connect-probing (which a wedged listener can fool).
- **Read-only proxy:** the MCP proxy never writes or deletes on disk — it *skips*
  dead endpoints/sentinel records, never prunes them. Only sessions mutate (and
  compact the Windows sentinel), each under its own lifetime lock, touching only
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
  - **shutdown unlinks only its own socket**: A leaks, B rebinds the path, A's
    shutdown leaves B's socket intact (inode guard);
  - **restart-then-wait**: `restart_service` (gen `G`) then
    `wait_for_healthy(after_generation = G)` does **not** return on the pre-restart
    Healthy — only on the new run;
  - **model under TUI backpressure**: a full `ui_tx` does not stop status
    transitions from reaching the model;
  - **Windows endpoint selection** (`cfg`-gated): `ControlEndpoint` picks the named
    pipe; sentinel-index record round-trips, the proxy **skips** records that fail
    to connect (never edits the file), and session startup/shutdown compacts it.
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
   `ServiceControlSink`, which cannot express `SendInput`/`ResizeAll`; see #8). ✅
4. **Model is scheduler-authoritative:** the core model is written by the scheduler
   (via `SessionModelWriter`) from its own transitions — one lifecycle model, not a
   second reducer re-deriving it. ✅
5. **Snapshot models desired vs execution separately**, plus `run_generation`, so
   control APIs are unambiguous and `wait_for_healthy` can be race-free. ✅
6. **TUI consolidation (M4) is required**, not optional — it removes the duplicate
   model. The TUI keeps view state; domain state moves to the model. ✅
7. **Transport abstraction from M1** (`ControlEndpoint`): Unix sockets / macOS,
   Windows named pipes; **no plain TCP** (loopback-TCP only as a last resort with a
   per-session auth token). "Socket-only" is a Unix/macOS invariant; Windows uses a
   verified sentinel index. Windows named-pipe support is its **own milestone
   (M1-Windows)** gating Windows control-plane release; until it lands the Windows
   binary runs control-disabled (never half-working). ✅
8. **Capability-split model + ports (compiler-enforced).** The model is a private
   `Inner` behind two handles: a **possession-scoped** `SessionModelWriter` (no
   public constructor, `!Clone`, built only by `model::channel()` and moved into the
   scheduler future — *not* a `pub(in …)` path, which can't name a sibling module)
   and a `Clone` `SessionModelReader` (read + `subscribe()`) given to every adapter.
   Adapters affect state only through a send-only **`ServiceControlSink`** — through
   which `SendInput`/`ResizeAll` are not even expressible, so "no raw input
   forwarding" is type-enforced, not policy (the TUI alone holds the full
   `TuiCommandSink`). Lifecycle changes go through transition methods that update
   runtime **and** model together (no direct `state =`), so "forgot to sync" is
   impossible, not just discouraged. The core depends on no transport/protocol
   crate; `micromux-control` owns `ControlEndpoint` + `ControlClient`/`ControlServer`,
   so the same protocol boundary serves Unix sockets, Windows pipes, and a future
   daemon. ✅

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
   `Reader`/`Writer` + `ServiceControlSink`/`TuiCommandSink`; `start_with_handles` +
   `Handles`; `SessionChange`. No behavior change.
2. **M1** `micromux-control` + endpoint adapter for **Unix/macOS** (deterministic
   name, lifetime-lock dance, `Describe`, transport abstraction) (+ optional `ctl`).
3. **M4** fold the TUI onto the model; delete the duplicate domain reducer —
   **validates model completeness** against the hardest consumer.
4. **M1-Windows** named-pipe transport + sentinel index behind the M1 abstraction.
   **Gates advertising Windows control-plane support**, so it lands before M2 *if*
   Windows parity is required at release; until then the Windows binary runs with
   the control plane **auto-disabled** (TUI works normally — never half-working).
5. **M2** `micromux-mcp` + `micromux mcp` with the core tool set.
6. **M3** `wait_for_healthy` (generation-aware), config `name`, docs/agent snippets,
   optional log streaming.

Shipping M2 before M4 is acceptable if the agent loop is urgent, but it knowingly
carries duplicate domain state longer — so the default is M4 first. M4 is required
either way to reach one model, many adapters.
