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
- No replacement for the TUI. The TUI and the agent are peer frontends.
- Not a generic remote-exec surface. Tool surface stays small and declarative.
- **No raw input forwarding.** Every tool is typed/structured. We intentionally
  do *not* expose `Command::SendInput` (which writes raw bytes to a *service's*
  PTY stdin — not micromux's own TUI keys; the latter never pass through a
  `Command` at all). Keeping the surface typed avoids a stdin/keystroke channel
  entirely.

---

## Why this is mostly plumbing, not new features

micromux is already split into a frontend-agnostic core and a command/event
interface (`crates/micromux/src/lib.rs`, `scheduler/types.rs`):

```
Micromux::start(ui_tx, commands_rx, shutdown)
   scheduler ──Event──▶  ui_tx        Started, LogLine, Healthy/Unhealthy,
                                       Exited, HealthCheck*, Disabled, ClearLogs
   scheduler ◀─Command── commands_rx  Restart, RestartAll, Enable, Disable,
                                       SendInput, ResizeAll
```

The TUI (`micromux-tui`) is just one consumer of this. The agent's entire
wishlist maps almost 1:1 onto existing `Command`/`Event`/`State` types. So the
work is: **add a second frontend (a control socket) + a thin MCP proxy**, plus
one enabling refactor so the core can support more than one concurrent frontend.

### Two facts from the current code that shape the design

1. **Service logs live in the TUI, not the core.** `micromux-tui/src/state.rs`
   holds `logs: AsyncBoundedLog` per service and `reducer::apply` materializes
   state from the event stream. The scheduler emits `LogLine` and forgets. A
   control socket that serves `get_logs` needs that buffer to live somewhere it
   can read.
2. **`ui_rx` is a single-consumer `mpsc`** — today only one frontend can exist.
   `commands_tx` is already multi-producer `mpsc`, so *sending* commands from a
   second frontend is free; only *events* need fan-out.

---

## Target topology

```
Claude / Codex ──stdio(JSON-RPC)──▶  micromux mcp     (thin proxy; read-only on disk, no supervision state)
                                          │  derives the socket path from cwd's config, or scans the socket dir
        ┌─────────────────────────────────┼──────────────────────────────────┐
        ▼                                  ▼                                   ▼
  $XDG_RUNTIME_DIR/micromux/        session A control socket           session B control socket
  ├─ a1b2c3.sock  ◀──────────────── (unix socket; name = hash(         (unix socket)
  └─ d4e5f6.sock                     canonical config path))                  ▲
                                            ▲  query + commands + Describe     │
                                   micromux (TUI) in proj-A          micromux (TUI) in proj-B
```

Two transports, deliberately different:

- **Agent ↔ MCP server: stdio.** MCP servers in agent configs are *spawned by
  the client* over stdio (not pre-existing endpoints the client dials). One
  process per agent session. This is why "every micromux is a server on a port"
  does not fit, and is where the port-conflict worry comes from.
- **MCP server ↔ micromux sessions: Unix domain sockets**, keyed by filesystem
  path. N sessions never collide, no port allocation, user-permission scoped, no
  network. **The port-conflict problem disappears entirely.** The **socket is the
  only on-disk artifact** — its name is derived deterministically from the
  project's config path, and all session metadata is fetched live via a
  `Describe` request, so there is no separate registry file to drift out of sync,
  race against, or leak. The socket dir is just where sockets live, not a store of
  JSON records.

Rejected alternative — *each session is its own HTTP/SSE MCP server*: agent
config needs a stable URL; a per-session ephemeral port has none, reintroducing
the discovery + allocation problem the proxy already solves for free.

---

## Component breakdown

| Component | Crate | Responsibility |
|---|---|---|
| Session model + event hub | `micromux` (core) | Own authoritative per-service state + bounded logs; broadcast events. The enabling refactor. |
| Control protocol + client | `micromux-control` (new) | Wire types, `Describe`/discovery, socket-path derivation, async client. Shared by server + MCP. |
| Control socket frontend | `micromux-cli` | Bind the socket (stale-socket dance) and unlink on exit; serve it against the core model. |
| `micromux ctl` (optional) | `micromux-cli` | Tiny CLI client that dogfoods the socket; validates the protocol; nice for humans/scripts. |
| MCP server | `micromux-mcp` (new lib) + `micromux mcp` subcommand | Discover sessions, expose MCP tools, proxy to sockets. |

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

### M0 — Enabling refactor: core-owned session model + event hub

**No user-visible behavior change. TUI unchanged.** This is the only structurally
interesting part.

Today `Micromux::start` hands a single `ui_tx` to the scheduler and the TUI owns
the only receiver. Introduce an internal **hub** between the scheduler and the
external frontend(s):

- In `Micromux::start`, create an internal channel `hub_tx`/`hub_rx` and pass
  `hub_tx` to the scheduler as its `ui_tx` (scheduler stays untouched).
- Spawn a **hub task** that drains `hub_rx` and, for each `Event`:
  1. updates a core-owned `SessionModel`,
  2. forwards a clone to the external `ui_tx` (the TUI) — using `try_send` for
     `LogLine` to preserve today's backpressure semantics,
  3. publishes to a `tokio::sync::broadcast` for live socket subscribers.
- Add `#[derive(Clone)]` to `Event` (all fields are already `Clone`) so it can
  fan out to multiple sinks.

`SessionModel` is the materialized view the control plane reads. It is the same
information the TUI's `reducer::apply` already computes — extract that logic into
a shared reducer in the core and reuse it:

```rust
// crates/micromux/src/model.rs  (sketch)
pub struct ServiceSnapshot {
    pub id: ServiceID,
    pub name: String,
    pub state: State,                 // Pending/Starting/Running{health}/Disabled/Exited/Killed
    pub open_ports: Vec<u16>,
    pub healthcheck_configured: bool,
    pub last_exit_code: Option<i32>,
    pub uptime: Option<Duration>,     // since last Started
    pub restart_policy: RestartPolicy,
}

pub struct SessionModel { /* Arc<RwLock<…>> over per-service snapshot + BoundedLog + last HC attempt */ }

impl SessionModel {
    pub fn apply(&self, event: &Event);                 // shared reducer
    pub fn services(&self) -> Vec<ServiceSnapshot>;
    pub fn logs(&self, id: &ServiceID, tail: Option<usize>) -> Vec<String>;
    pub fn healthcheck(&self, id: &ServiceID) -> Option<HealthAttempt>; // command, output, success, exit_code
}
```

New public API on `Micromux` (additive, opt-in — existing `start` keeps working):

```rust
pub struct Handles { pub model: SessionModel, pub events: broadcast::Sender<Event>, pub commands: mpsc::Sender<Command> }
pub async fn start_with_handles(&self, ui_tx, commands_rx, shutdown) -> (impl Future<Output=…>, Handles);
```

**Why query-the-model + broadcast-for-liveness, not pure broadcast:**
`broadcast` drops messages for lagging receivers. If a frontend lags we must not
lose *log content*. Putting content in the queryable `SessionModel` means the
broadcast only needs to carry "something changed" liveness; a lagging subscriber
re-queries and is still correct.

Reducer dedup note: the TUI keeps its own `state::Service`/`reducer` for now
(some duplication with `SessionModel`, but non-breaking). A later, optional pass
can collapse the TUI onto `SessionModel`. Don't block M0 on that.

**Tests:** model reducer unit tests mirroring `reducer.rs` cases (Started →
Running, LogLine append/replace/clear, Healthy/Unhealthy, Exited exit code,
Disabled); a hub test asserting an event reaches both the TUI mpsc and a
broadcast subscriber and is reflected in `model.services()`.

**Acceptance:** TUI behaves identically; `cargo test` green; new model/hub
covered.

### M1 — Control plane: the per-session control socket

Add a second frontend in `micromux-cli` driven off the M0 handles. **Default on,
opt-out two ways (decided):** a CLI flag (`--no-control`) and a config-file
setting (top-level `control: { enabled: false }`). Also auto-disabled if no
runtime dir is resolvable. The CLI flag wins over the config setting when both
are present.

On startup (the session is the *only* writer on disk — the proxy never writes):
1. Resolve the **runtime dir** (see spec) and ensure `…/micromux/` exists (`0700`).
2. Compute the socket path deterministically from the canonical config path:
   `…/micromux/<hash>.sock`. Bind it via the **stale-socket dance**: on
   `EADDRINUSE`, `connect()`-probe the existing path — if it *answers*, another
   micromux already owns this project (refuse, or take a `<hash>-<pid>.sock`
   variant); if it *refuses*, the file is a corpse → `unlink` and rebind.
3. Spawn an accept loop (one task per connection): each connection speaks the
   control protocol against the `SessionModel` (queries), the `commands` sender
   (mutations), and the `events` broadcast (subscribe). `Describe` returns the
   session's identity (pid, start-time, name, working_dir, config_path, services).

There is **no separate registry file**: the socket is the only artifact, so it
can never disagree with "is there a listener." Binding *is* the advertisement —
there is no window where the session is listed but not yet listening.

On shutdown (hook the existing `CancellationToken`): unlink the socket. A crash
that skips this leaves an inert socket file that the next same-project start
reclaims via the dance — **no background reaper, no cross-process healing.**

Optional in this milestone: `micromux ctl {ls|logs|restart|…}` — a tiny client
in the same binary. Not required, but it exercises the protocol end-to-end with
no MCP/agent in the loop and gives humans/scripts a CLI.

**Tests:** integration test that boots the core against a temp config, connects
to the socket, and asserts `list_services` / `restart` / `get_logs` / `Describe`
behave; the socket is created and unlinked on shutdown; the stale-socket dance
unlinks a dead predecessor and rebinds, and refuses (or pid-suffixes) when a live
peer holds the path.

**Acceptance:** with micromux running, a raw socket client (or `micromux ctl`)
can list services, tail logs, and restart a service; the socket is cleaned up on
exit, and a leaked one is reclaimed on the next same-project start.

### M2 — MCP server (`micromux mcp`)

New `micromux-mcp` lib + `micromux mcp` subcommand. Use the official **`rmcp`**
Rust MCP SDK for stdio/JSON-RPC plumbing. Stateless: connect to a session socket
per tool call (cheap), hold no supervision state.

**All MCP code is feature-gated behind a default-on `mcp` feature** and lives in
one isolated module gated at the top (`#[cfg(feature = "mcp")] mod mcp;` in
`micromux-cli`, backed by the optional `micromux-mcp` dep). The clap `Mcp`
subcommand variant and its dispatch arm are `#[cfg(feature = "mcp")]` too, so with
the feature off the subcommand, the module, and `rmcp` all vanish at compile time
and nothing else in the CLI references them. The control plane (M0/M1) is *not*
gated — it has no `rmcp` dependency and is useful on its own (e.g. `micromux ctl`).

Session discovery + selection (see spec). Tools (v1):

| Tool | Args | Returns | Backed by |
|---|---|---|---|
| `list_sessions` | — | sessions: id, name, cwd, pid, services | socket-dir scan + `Describe` |
| `list_services` | `session?` | name, state, health, ports, uptime, restart policy, last exit | `SessionModel::services` |
| `get_logs` | `service`, `session?`, `tail?` | recent log lines | `SessionModel::logs` |
| `restart_service` | `service`, `session?` | ack | `Command::Restart` |
| `restart_all` | `session?` | ack | `Command::RestartAll` |
| `enable_service` | `service`, `session?` | ack | `Command::Enable` |
| `disable_service` | `service`, `session?` | ack | `Command::Disable` |
| `get_health` | `service`, `session?` | last probe: success, exit code, command, output | last HC attempt |

**Tests:** spin up the core + socket in-process, point the MCP tool handlers at
it, assert each tool's behavior. A discovery test with two fake session sockets
(stub listeners) asserting cwd-derived path selection, explicit `session`
override, and that a refusing (dead) socket is skipped.

**Acceptance:** `micromux mcp` configured in Claude Code lists/controls a running
session with zero `session` args when launched in that project's dir.

### M3 — Ergonomics & polish

- **`wait_for_healthy(service, timeout, session?)`** — the highest-value tool;
  exactly what agents want ("restart api and tell me when it's back") and
  annoying to build from polling. Implement MCP-side by polling `list_services`
  over the socket (~250ms) until healthy/exited/timeout. (Later: switch to the
  `subscribe` event stream for push instead of poll.)
- **Optional config `name:`** — top-level identifier surfaced as the session id;
  add to the v1 parser (`config/v1.rs`) and known top-level keys. Falls back to
  `basename(working_dir)`, disambiguated by pid.
- **Optional log streaming** — a `subscribe` socket request + a `follow_logs`
  tool, once there's a need.
- Docs: README section + agent config snippets (Claude Code + Codex) for the
  `~/dev/configuration` repo.

---

## Detailed specs

### Runtime dir resolution

- **Linux:** `directories::ProjectDirs::runtime_dir()` → `$XDG_RUNTIME_DIR/micromux/`
  (already on `directories = "6"`).
- **macOS / Windows:** `runtime_dir()` is `None`. Fall back to
  `std::env::temp_dir()/micromux-<uid>/` created with `0700`.
- If none resolvable, log a warning and run with the control plane disabled
  (TUI still works).

### Socket layout & the `Describe` handshake

The **socket is the only on-disk artifact** — there is no registry JSON to drift
out of sync. Layout under the runtime dir:

- The path is **deterministic from the project**: `…/micromux/<hash>.sock`, where
  `<hash>` is a short digest of the *canonical config path* (the same config
  micromux's `find_config_file` resolves). Session and proxy derive the identical
  path from the same input, so the common one-session-per-project case needs **no
  enumeration**: the proxy computes the path and connects.
- A rare concurrent second instance on the same config → `<hash>-<pid>.sock`.
- All session identity/metadata is returned *live* by a `Describe` request, never
  stored in a file:

  ```
  Describe → { schema_version, pid, start_time, name, working_dir,
               config_path, services: [..], micromux_version }
  ```

  `pid` + `start_time` together defend against PID reuse for the `-<pid>` variant.
  `name` is the config `name:` (M3) else `basename(working_dir)`.

Why socket-only beats a `<pid>.json` + socket pair: it removes the entire
file/socket consistency hazard at the source — no "metadata file without a
socket," no write-before-bind race, no stale metadata. The single artifact's
*connectability* is the only source of truth (see invariant below).

**Windows note:** AF_UNIX is unreliable under tokio there; fall back to a named
pipe (`\\.\pipe\micromux-<hash>`) or loopback TCP whose ephemeral port `Describe`
returns. Enumeration then needs a pipe-name convention or a small index file — a
Windows-only concern; Unix stays socket-only.

### Liveness = connectability, not latency (invariant)

The rule that keeps discovery robust: **a session's liveness is decided by the
kernel's connection result, never by how fast it replies.**

- `connect()` → `ECONNREFUSED` / `ENOENT` ⇒ unambiguously dead. (A unix socket
  file outlives its process; an orphaned one refuses connections.)
- A session that is alive but *busy* still accepts at the kernel level (the listen
  backlog is in-kernel), so it connects fine even while its loop is slow.
- A timeout governs only "how long I wait for a *reply* to a request I already
  delivered." Hitting it returns a `session busy` error to the agent — it **never
  deletes or de-lists the session.**

Corollary: a laggy session can never be "healed away." Only connection-level
failure (refused / gone / dead-pid+start-time) marks a session absent.

### Control wire protocol (`micromux-control`)

Newline-delimited JSON, request/response. One enum each, `serde`-tagged:

```rust
enum Request {
    Describe,                        // session identity for discovery (see spec)
    ListServices,
    GetLogs { service: ServiceID, tail: Option<usize> },
    GetHealth { service: ServiceID },
    Restart { service: ServiceID },
    RestartAll,
    Enable { service: ServiceID },
    Disable { service: ServiceID },
    Subscribe,                       // M3: switches the conn to server-push events
}
enum Response {
    Description(SessionInfo),        // pid, start_time, name, working_dir, config_path, services, version
    Services(Vec<ServiceSnapshot>),
    Logs { lines: Vec<String> },
    Health(Option<HealthAttempt>),
    Ack,
    Error { message: String },
    Event(EventDto),                 // only after Subscribe
}
```

Keep it request/response for M1–M2 (no streaming); `wait_for_healthy` polls.
Add `Subscribe` server-push in M3. Snapshot/health/log DTOs are plain
`serde::Serialize` mirrors of core types so the core does not depend on the wire
crate.

### Session selection (MCP server) — read-only, connect-to-verify

The proxy never mutates the filesystem; it only connects.

1. Explicit `session` arg → resolve to its socket (scan the dir + `Describe` to
   match name/pid); error if it does not answer.
2. Else `MICROMUX_SESSION` env → same.
3. Else **derive from cwd**: run micromux's own `find_config_file` upward from the
   proxy's cwd (the project root the client launched it in), canonicalize, hash,
   and connect to `…/micromux/<hash>.sock`. Connects ⇒ that's the session
   (`Describe` for details). Refused/absent ⇒ "no running micromux for this
   project; start one." **Zero enumeration on the happy path.**
4. `list_sessions` and disambiguation scan the socket dir, connect to each,
   `Describe` the ones that answer, and silently skip the ones that refuse.

Dead/orphan sockets are simply invisible (refused → skipped). The proxy deletes
nothing; reclaiming a dead socket is the next same-project session's job, via the
bind-time dance.

---

## Crate / module layout

```
crates/
  micromux/                 # core
    src/model.rs            # NEW: SessionModel + shared reducer (extracted from tui reducer)
    src/lib.rs              # NEW: start_with_handles(), Handles; derive Clone on Event
  micromux-control/         # NEW lib: wire Request/Response + Describe, socket-path derivation, async Client, dir resolution
  micromux-cli/
    Cargo.toml              # [features] default = ["mcp"]; mcp = ["dep:micromux-mcp"]
    src/control/mod.rs      # NEW: socket server frontend; bind via stale-socket dance, unlink on shutdown
    src/control/ctl.rs      # OPTIONAL: `micromux ctl` client subcommand (not feature-gated)
    src/mcp.rs              # NEW: `#[cfg(feature = "mcp")]` thin shim → micromux-mcp; gated at the top
    src/options.rs          # control flags; `ctl` subcommand; `Mcp` variant under #[cfg(feature = "mcp")]
    src/main.rs             # wire control frontend off start_with_handles; dispatch subcommands
  micromux-mcp/             # NEW lib (optional dep): rmcp server, tool defs, discovery; driven by `micromux mcp`
```

### New dependencies

- `micromux-mcp`: `rmcp` (official MCP SDK), `tokio`, `serde`/`serde_json`,
  `micromux-control`. Pulled in by `micromux-cli` as an **optional** dependency
  enabled by the default-on `mcp` feature (`mcp = ["dep:micromux-mcp"]`), so `rmcp`
  is built only when the feature is on.
- `micromux-control`: `serde`/`serde_json`, `tokio` (UnixListener/UnixStream),
  `directories` (already used).
- `micromux` core: `tokio::sync::broadcast` (tokio already present).

Mind the workspace lints (`unwrap_used`, `expect_used`, `panic`, `indexing_slicing`
all denied) — protocol parsing and socket handling must be fully fallible.

---

## Lifecycle, security, robustness

- **Socket perms:** `0600`; runtime dir `0700`. No network, no other-user access
  by construction.
- **Liveness invariant:** a session is alive iff its socket connects; lag never
  de-lists it (see spec). Reaping keys on connection failure, not reply latency —
  so a busy session is never mistaken for a dead one.
- **Read-only proxy:** the MCP proxy never writes or deletes on disk. Only
  sessions mutate, and each only ever touches *its own* socket path —
  single-writer-per-file, so concurrent proxies and sessions cannot race on
  cleanup (and `unlink` of a provably-dead path is idempotent regardless).
- **Cleanup:** unlink the socket on graceful shutdown (hook the existing
  `CancellationToken`). No reaper task: a crash-leaked socket is inert and is
  reclaimed by the next same-project start's bind-time dance.
- **Crash safety:** a killed micromux leaves only an inert socket file; it refuses
  connections, so the proxy treats it as absent and never has to act on it.
- **Reach is currently-open sessions only:** closing the TUI exits the process,
  stops its services (by design), and removes the socket. The agent can act on
  exactly the sessions a human has open right now — a property of micromux's
  ephemeral model, not a bug. Changing it is the daemon decision (see
  Alternatives), not a plumbing tweak.
- **Backpressure:** hub forwards `LogLine` to the TUI with `try_send` (today's
  behavior); the broadcast is liveness-only so lag never loses log content.
- **Opt-out:** `--no-control` (CLI) or `control: { enabled: false }` (config)
  fully disables the socket.
- **Tool-surface discipline:** read/observe + restart are the entire surface.
  Every tool is typed — no raw stdin/keystroke channel, no arbitrary exec.

---

## Testing strategy

- **Unit:** `SessionModel` reducer (mirror `reducer.rs` cases); wire protocol +
  `Describe` serde round-trip; socket-path derivation from a config path;
  selection algorithm.
- **Integration:** boot core against a temp config (pattern already used
  throughout `scheduler.rs` tests) → drive the socket → assert list/logs/restart;
  socket bind + unlink-on-exit; the stale-socket dance (unlink dead predecessor,
  refuse/pid-suffix on a live peer).
- **MCP:** in-process core+socket, call tool handlers directly, assert outputs
  and cwd-based discovery.
- **Manual:** `micromux mcp` in Claude Code against the `examples/demo` config.

---

## Alternatives considered (discovery / control transport)

- **Registry JSON file + self-healing reaper (earlier draft) — superseded** by
  the socket-only design above. The separate metadata file was the brittle part:
  write-before-bind races, a metadata file that can outlive (or precede) its
  socket, and a reaper that could mis-fire on a merely-laggy session. Deleting the
  file and fetching identity via `Describe` removes the whole failure class.
- **mDNS / Bonjour — no.** Cross-*host* multicast discovery: wrong scope
  (everything here is same-host/same-user), it would expose dev services on the
  LAN, and it needs avahi/bonjour. Solves a problem we don't have and adds ones we
  don't want.
- **CRDT — no (category error).** CRDTs converge concurrent conflicting writes to
  shared replicated state. Here each session owns its own state, the proxy is a
  read-only observer, and commands are point-to-point RPCs — nothing to converge.
  The socket dir is already a single-writer-partitioned set (conflict-free for
  free); CRDT machinery buys nothing.
- **D-Bus — closest to a real local "self-discovery bus," but no.** It gives
  naming + discovery + introspection on Linux, but it's Linux-only (kills the
  macOS/Windows story), pulls a heavy `zbus` dependency, and assumes a session bus
  that is often absent (containers / CI / headless). Poor fit for a cross-platform
  CLI.
- **Single daemon (Docker model) — deferred, not rejected.** It *is* more robust
  for discovery (one well-known socket, one source of truth, nothing to enumerate
  or reap). But it changes what micromux *is*: a background service to install and
  run (systemd), daemon↔CLI version skew, and an inverted lifecycle where services
  outlive the TUI. It also complicates the careful PTY/signal/env handling the
  in-process model gets right — the PTY master must live where it is rendered, so a
  daemon would need fd-passing (`SCM_RIGHTS`) or would stop rendering PTYs. Wrong
  trade for an ephemeral per-project tool today.
  **De-risk:** the per-session wire protocol is identical whether the proxy dials
  one socket or many, so this can become an *optional* daemon later (for
  detach/persistence) without a protocol change. Choosing sockets now does not
  foreclose it.

## Decisions (settled)

1. **MCP packaging:** `micromux mcp` subcommand, one binary, behind a default-on
   `mcp` Cargo feature; all MCP code in one feature-gated module so `rmcp` is
   absent from `--no-default-features` builds. ✅
2. **Control plane:** default on; opt-out via CLI flag (`--no-control`) *or*
   config setting (`control: { enabled: false }`); CLI flag wins. ✅
3. **No `send_input`:** the surface is fully typed — no raw stdin/keystroke
   forwarding. ✅

## Open questions to confirm before coding

1. **TUI consolidation onto `SessionModel`** now (bigger M0) or deferred
   (duplicate reducer for a while)? Default: defer.
2. **Windows transport** — Unix is socket-only; Windows needs a named-pipe or
   loopback-TCP variant plus an enumeration convention (see spec). Build it day
   one, or ship Unix-only first? Default: Unix-first, add Windows when needed.

---

## Suggested sequencing

1. **M0** core model + hub (no behavior change) — unblocks everything, low risk.
2. **M1** `micromux-control` + socket frontend (deterministic path + dance,
   `Describe`) (+ optional `ctl`).
3. **M2** `micromux-mcp` + `micromux mcp` with the core tool set.
4. **M3** `wait_for_healthy`, config `name`, docs/agent snippets, then optional
   streaming / `send_input`.

Ship M0–M2 to get a working agent control loop; M3 is ergonomics on top.
