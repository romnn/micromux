# Approach

I’ll first map the repo structure and identify the real entry points (CLI command wiring, process/spawn logic, TUI/render loop, config/state). Then I’ll read `TODO.md` and the core modules to do a deep review for correctness, safety, and architectural gaps. Finally I’ll write a concrete, prioritized implementation plan for a “rock solid v1” (including refactors, tests, observability, and UX).

I’m going to create a small task checklist and then start by scanning the codebase and `TODO.md`.

I’m going to start by mapping the repo: locate the CLI entrypoint(s), core process/multiplexing code, and read `TODO.md`. I’ll use a codebase search to quickly find where commands are defined and where terminal processes are spawned/managed.
Next I’ll read `TODO.md` and then walk the core crates (`micromux`, `micromux-cli`, `micromux-tui`) to understand the current runtime architecture: config parsing, service graph, scheduler/state machine, process spawning/IO, and the TUI loop. I’ll use a few targeted file reads to build an accurate mental model before writing the v1 plan.
I’ve found your current MVP is structured as a Rust workspace with three crates: [micromux-cli](cci:9://file:///home/roman/dev/micromux/crates/micromux-cli:0:0-0:0) (entrypoint + config loading), [micromux](cci:9://file:///home/roman/dev/micromux:0:0-0:0) (service graph + scheduler + process start/health), and [micromux-tui](cci:9://file:///home/roman/dev/micromux/crates/micromux-tui:0:0-0:0) (event loop + rendering + in-memory UI state). Next I’ll read the remaining core modules (service/config/graph/health/render/state) to identify concrete correctness bugs and architectural gaps, then I’ll synthesize a v1 scope + a prioritized implementation plan grounded in your [TODO.md](cci:7://file:///home/roman/dev/micromux/TODO.md:0:0-0:0).





I’m going to read the remaining core modules (`config/*`, `graph`, `health_check`, `bounded_log`, the TUI `state` and input handling) so the review and v1 plan are grounded in what’s actually implemented today—especially around process lifecycle, restart semantics, and log/IO handling.


I’m going to finish mapping the remaining “runtime critical” files ([shutdown.rs](cci:7://file:///home/roman/dev/micromux/crates/micromux/src/shutdown.rs:0:0-0:0), [diagnostics.rs](cci:7://file:///home/roman/dev/micromux/crates/micromux/src/diagnostics.rs:0:0-0:0), and the rest of the TUI `App` input handling) so the review catches lifecycle/shutdown and error-handling issues. Then I’ll write up: (1) critical bug/soundness findings, (2) architectural gaps, and (3) a concrete v1 implementation plan incorporating [TODO.md](cci:7://file:///home/roman/dev/micromux/TODO.md:0:0-0:0).


# Current MVP: what you already have

## Runtime data flow (today)
- **[micromux-cli](cci:9://file:///home/roman/dev/micromux/crates/micromux-cli:0:0-0:0)**
  - Parses CLI flags, locates and parses [micromux.yaml](cci:7://file:///home/roman/dev/micromux/micromux.yaml:0:0-0:0), constructs [Micromux](cci:2://file:///home/roman/dev/micromux/crates/micromux/src/lib.rs:23:0-33:1), starts TUI + scheduler concurrently, and uses a shared `CancellationToken` for shutdown.
- **[micromux](cci:9://file:///home/roman/dev/micromux:0:0-0:0)**
  - Builds [Service](cci:2://file:///home/roman/dev/micromux/crates/micromux-tui/src/state.rs:31:0-40:1) structs from config.
  - Runs a **scheduler loop** with:
    - `service_state: HashMap<ServiceID, State>` (authoritative *actual* state)
    - [schedule_ready()](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/scheduler.rs:211:0-312:1) that scans all services and calls [start_service()](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/scheduler.rs:110:0-209:1) when deps are satisfied
  - [start_service()](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/scheduler.rs:110:0-209:1) spawns an `async_process::Child`, pipes `stdout/stderr`, and sends a `Started { stdout, stderr }` event to the UI.
  - Health checks are separate processes; their results are translated into `Healthy/Unhealthy` events.
- **[micromux-tui](cci:9://file:///home/roman/dev/micromux/crates/micromux-tui:0:0-0:0)**
  - Maintains its own state ([state::State](cci:2://file:///home/roman/dev/micromux/crates/micromux-tui/src/state.rs:43:0-49:1)) and renders via ratatui.
  - On `Started`, it spawns tasks that read `stdout/stderr` line-by-line and appends to a bounded log buffer.

This is already a credible MVP shape: config → scheduler → events → UI state → render.

# Critical bugs / soundness issues (must fix before v1)

## 1) **Dependency graph construction is incorrect (order-dependent)**
In [graph::ServiceGraph::new](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/graph.rs:11:4-45:5), you validate dependencies while iterating services, but you only `add_node()` for services you’ve already visited. If a service depends on a service that appears *later* in the map, it incorrectly fails with “depends on unknown”.
- **Impact**: valid configs fail depending on YAML ordering.
- **Fix direction**: first add *all nodes*, then add edges + validate, then run cycle check.

## 2) **Termination token is shadowed; “terminate” wiring is effectively broken**
In [scheduler::start_service(...)](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/scheduler.rs:110:0-209:1) you accept `terminate: CancellationToken` but then immediately do:
- [let terminate = CancellationToken::new();](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/graph.rs:11:4-45:5)
This **discards** the provided token. Additionally, the caller creates [terminate](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/service.rs:108:4-133:5) in [schedule_ready()](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/scheduler.rs:211:0-312:1) but **doesn’t store it anywhere**, so there is currently no way to cancel a service instance.
- **Impact**: disable/restart cannot be implemented correctly; shutdown relies on killing the child, but you don’t have control handles per service.
- **Fix direction**: store per-service runtime handles (terminate token + child handle + pid/pgid) in an owned runtime state map.

## 3) **RestartPolicy::OnFailure attempt accounting is not sound**
Current flow:
- On exit, [update_state](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/scheduler.rs:314:0-369:1) creates `State::Exited { restart_policy: services[service_id].restart_policy.clone() }`
- In [schedule_ready](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/scheduler.rs:211:0-312:1), you decrement `remaining_attempts` inside the state’s `restart_policy`
But on the next exit, you clone the original policy again, resetting attempts.
- **Impact**: `OnFailure` attempts don’t reliably decrease across restarts; behavior is surprising/non-deterministic.
- **Fix direction**: keep “remaining attempts” as runtime state (not re-cloned from config on every exit), or keep the original policy immutable and track attempts separately.

## 4) **Config parsing is incomplete (many fields silently ignored)**
Your config model supports:
- `depends_on`, `environment`, `env_file`, `ports`, `restart`, etc.
…but [config/v1.rs](cci:7://file:///home/roman/dev/micromux/crates/micromux/src/config/v1.rs:0:0-0:0) currently returns:
- `depends_on: vec![]`
- `environment: empty`
- `ports: vec![]`
- `restart: None`
So dependencies/restarts/ports/environment do not work, yet the rest of the system assumes they do.
- **Impact**: “works on my config” but fundamentally not v1-ready; users cannot rely on config semantics.
- **Fix direction**: implement parsing + diagnostics + validation for all v1 keys you intend to support.

## 5) **Diagnostics are not displayed to the user**
[diagnostics::Printer::emit](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/diagnostics.rs:61:4-89:5) currently returns `Ok(())` without emitting to stderr (the actual [term::emit](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/diagnostics.rs:61:4-89:5) code is commented out).
Also in CLI, on config parse error you `return Ok(())` **without printing** the accumulated diagnostics.
- **Impact**: config errors become silent / invisible.
- **Fix direction**: re-enable [codespan_reporting::term::emit](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/diagnostics.rs:61:4-89:5) and ensure CLI always prints diagnostics before exit.

## 6) **TUI key handling bug: duplicate `w` binding**
In [micromux-tui/src/lib.rs](cci:7://file:///home/roman/dev/micromux/crates/micromux-tui/src/lib.rs:0:0-0:0), `KeyCode::Char('w')` is used twice (wrap and follow-tail). Only one can ever win.
- **Impact**: one feature is unreachable.
- **Fix direction**: separate keys (e.g. `w` for wrap, `t` for tail), and add a help footer entry.

## 7) **No Ctrl-C / SIGTERM handling in the running app**
You have a [shutdown.rs](cci:7://file:///home/roman/dev/micromux/crates/micromux/src/shutdown.rs:0:0-0:0) module with signal handling, but the app uses `CancellationToken` and never hooks OS signals to cancel it.
- **Impact**: ctrl-c won’t reliably shut everything down gracefully (depends on terminal/TUI behavior).
- **Fix direction**: install signal handler in CLI and call `shutdown.cancel()`.

# Architectural gaps / “gaping holes” relative to your stated goal

## 1) Missing control plane (commands channel is unused)
[Micromux::start](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/lib.rs:73:4-106:5) creates `commands_tx/commands_rx` but nothing can send commands:
- TUI “restart/disable” functions only log (no command is sent).
- Scheduler command handling is stubbed.
- **Needed for v1**: a well-defined bidirectional interface:
  - UI → Engine: `Command`
  - Engine → UI: `Event` (including logs)

## 2) Log capture happens in the UI layer (wrong ownership for a dev tool)
Today the engine sends raw `ChildStdout/ChildStderr` to the UI, and UI spawns log reader tasks.
- **Impact**:
  - Headless mode becomes hard.
  - You can’t easily persist logs, multiplex logs, filter, tag, or apply backpressure centrally.
  - If UI crashes/exits early, you can lose output handling.
- **v1 direction**: engine owns IO capture; UI receives `Event::LogLine { service_id, stream, text }`.

## 3) Not using a PTY means you can’t “handle any sorts of terminal processes”
Piping stdout/stderr is not equivalent to a terminal:
- Many programs change behavior without a TTY (buffering, interactive prompts, color output).
- Your TODO already notes this: `portable-pty`.
- **v1 direction**:
  - Spawn services in a PTY by default (opt-out possible).
  - Support window resize propagation.
  - Optionally support “attach mode” where keystrokes go to the selected service.

## 4) State model is split/duplicated and not authoritative
There is:
- `micromux::service::Service { state, health, process }` (mostly unused)
- Scheduler `service_state: HashMap<ServiceID, State>` (actual truth)
- TUI `state::Service.exec_state` (derived display state)
- **v1 direction**:
  - Treat config as immutable “spec”.
  - Maintain a single engine-owned “runtime state” per service instance (pid, status, health, restarts, terminate handle).
  - UI keeps its own display state, updated from engine events (your TODO about decoupling fits well here).

# v1 target architecture (clean, extensible, obvious)

A solid structure (still close to your crates) is:

- **[micromux](cci:9://file:///home/roman/dev/micromux:0:0-0:0) (core)**
  - Config parsing/validation + diagnostics
  - Types: `ServiceSpec`, `HealthCheckSpec`, `RestartPolicy`, `DependencyCondition`
  - Graph building (order-independent) + validation
- **`micromux-engine` (or keep inside [micromux](cci:9://file:///home/roman/dev/micromux:0:0-0:0) as `engine` module)**
  - `Supervisor` / `Runtime`
    - Owns process spawning (PTY), termination, restart/backoff
    - Owns IO capture and emits structured events
    - Reconciliation loop: *desired vs actual* state machine (from TODO.md)
  - Command handling: enable/disable/restart, restart-all, shutdown
- **[micromux-tui](cci:9://file:///home/roman/dev/micromux/crates/micromux-tui:0:0-0:0)**
  - Pure UI: subscribes to engine events, sends commands
  - UI state is independent and event-driven (your TODO)
- **[micromux-cli](cci:9://file:///home/roman/dev/micromux/crates/micromux-cli:0:0-0:0)**
  - Wires config + engine + UI (and optional headless mode)
  - Installs signal handlers

# v1 scope (what “proper v1” should do)

## Engine / process supervision
- **Start/stop/restart/disable** services reliably.
- **Graceful shutdown**:
  - signal handling (Ctrl-C/SIGTERM)
  - terminate children (prefer process group)
  - timeout → hard kill
- **Restart policies**:
  - `Never`, `Always`, `UnlessStopped`, `OnFailure { max_attempts }`
  - add **restart backoff** to avoid tight crash loops
- **Health checks**:
  - status + last result + failure reason surfaced to UI
  - health checks cancelled when service stops
- **PTY support** (portable-pty):
  - consistent color + interactive capability
  - resize handling

## Config / UX
- Parse and validate:
  - `depends_on` (+ conditions)
  - `environment` + interpolation (`${VAR}`) + `shellexpand`
  - `env_file` / dotfiles parsing
  - `ports`, `restart`, `color`, `cwd` (likely needed), timeouts/intervals
- Great diagnostics (codespan) on invalid config.

## TUI (dev tool quality)
- Navigation between services and log view “sections” (your TODO).
- Log UX:
  - scrollbar fix
  - follow-tail toggle
  - wrap toggle (separate key)
  - vim-style search (TODO)
  - optionally “All services” aggregated view
- Show health checks (TODO): status, last run time, last error summary.

# Implementation plan (prioritized milestones)

## Milestone 1: Fix correctness + unblock core semantics (1–2 days)
- **Fix graph bug**: make dependency validation order-independent.
- **Fix diagnostics**: actually emit codespan diagnostics; ensure CLI prints them on error.
- **Wire OS signals → CancellationToken**.
- **Fix keybind conflict** (`w` duplicated).
- **Remove “silent” debug leftovers** that break UX (`dbg!(&config)`, logging `dbg!`s), and stop logging huge strings (e.g. full logs) at trace level.

## Milestone 2: Introduce an engine “Supervisor” with per-service runtime handles (2–4 days)
- Create an engine-owned structure:
  - `ServiceRuntime { desired_state, actual_state, child/pty handle, terminate token, restart_counters, last_exit, last_health }`
- Implement command handling:
  - `Disable`, `Enable`, `Restart(service)`, `RestartAll`, [Shutdown](cci:2://file:///home/roman/dev/micromux/crates/micromux/src/shutdown.rs:63:0-66:1)
- Implement a real desired-vs-actual reconcile loop (your TODO):
  - scheduler becomes a reconciliation loop rather than “scan-and-start”
  - ensures transitions are smooth and idempotent

## Milestone 3: Move IO/log capture into the engine (2–3 days)
- Engine reads PTY output (or stdout/stderr initially) and emits:
  - `Event::LogLine { service_id, stream, line }`
- UI no longer receives raw `ChildStdout/ChildStderr`.
- Add log fan-out:
  - bounded in-memory buffer per service
  - optional persisted log file per service/session

## Milestone 4: PTY support + multiplexing/attach mode (3–6 days)
- Replace `async-process` piping with `portable-pty`:
  - set TERM properly
  - handle resize events
- Implement “attach” mode:
  - UI routes keystrokes to selected service PTY (with an escape to return to UI control mode)
- This is the step that truly enables “any sorts of terminal processes”.

## Milestone 5: Config v1 completion (parallelizable with 2–4) (2–5 days)
- Implement parsing + validation for:
  - `depends_on`, `ports`, `restart`, `environment`, `env_file`
- Implement:
  - `shellexpand`
  - `${VAR}` interpolation rules (define precedence: env_file < config env < process env?)
  - dotfiles parsing (`.env`)
- Add config tests covering diagnostics and edge cases.

## Milestone 6: TUI polish to “production dev tool” (2–5 days)
- Switch between services/log sections (TODO).
- Fix scrollbar behavior fully (TODO).
- Add vim-style search (TODO).
- Show health checks (TODO).
- Add a help overlay / key cheat sheet.

## Milestone 7: Hardening + QA (ongoing, but gate for v1)
- Integration tests spawning fake services:
  - exits immediately, crash loops, long-running, noisy logs, interactive
- Stress tests:
  - many services, high log throughput, long sessions
- Observability:
  - structured tracing spans per service
  - metrics counters (optional) for restarts/exits/health failures

# Status
- **Completed**: deep repo mapping and identification of the highest-risk bugs/gaps + a concrete v1 milestone plan (grounded in [TODO.md](cci:7://file:///home/roman/dev/micromux/TODO.md:0:0-0:0)).
- **Next step**: I can start implementing **Milestone 1** (graph fix, diagnostics output, signal wiring, keybind fix) to stabilize the baseline before the engine refactor.

# Lock down before v1 TODO's

I’ll do a fresh, end-to-end architecture review by (1) mapping the current crate/module boundaries and runtime data flow (CLI → config/diagnostics → core scheduler/healthchecks → TUI), (2) identifying coupling/ownership issues and “missing abstraction” hotspots, and (3) proposing a small, high-leverage plan to “lock down” the architecture before the final lint/docs/tests pass. I’m going to inspect the relevant entry points and core modules first.



I’ll base the review on the actual code boundaries and call graph: `micromux-cli` currently owns process startup, config discovery + parsing + diagnostics emission, then spawns the `micromux` scheduler and the `micromux-tui` event loop. Next I’ll inspect `micromux`’s config/service/env/healthcheck modules and the TUI state/render split to evaluate separation of concerns and identify any “lock-down before polish” refactors that are worth doing.







# Architecture Review (Micromux CLI + Core + TUI)

## Current high-level structure (as it exists today)

- **`micromux-cli`**
  - Owns: CLI options parsing, signal handling, config discovery, config file I/O, diagnostics emission, logging setup, and wiring together TUI + engine.
  - Starts two async “subsystems”:
    - **Engine**: [Micromux::start_with_options(...)](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/lib.rs:59:4-89:5) (spawns scheduler)
    - **UI**: `micromux_tui::App::render()/run()`

- **`micromux` (core crate)**
  - Owns:
    - **Configuration layer**: `config::{from_str, v1, ConfigError->Diagnostics}`
    - **Diagnostics**: `diagnostics::Printer` and `ToDiagnostics`
    - **Runtime model**: [service::Service](cci:2://file:///home/roman/dev/micromux/crates/micromux/src/config/mod.rs:147:0-158:1) (resolved config + env + ports)
    - **Runtime engine**: [scheduler](cci:1://file:///home/roman/dev/micromux/crates/micromux/src/scheduler.rs:702:0-906:1) (event loop, restart/disable/enable, PTY spawning, log streaming, termination)
    - **Runtime sidecars**: `health_check` (spawning + supervision), `graph` (dependency graph), `env` (dotenv + expansion)

- **`micromux-tui`**
  - Owns:
    - **UI state**: `state::{State, Service, HealthCheckAttempt}`
    - **Rendering**: `render` (widgets, log view, scrollbar logic)
    - **Input**: `event::InputHandler`
  - Consumes `micromux::scheduler::{Event, Command}` and emits commands back to core.

## What’s already *good* (and worth preserving)

- **Clear integration boundary**
  - The `Event`/`Command` channel boundary between “engine” and “UI” is a strong separation point. It’s the most valuable architectural choice in the repo.

- **Config/diagnostics quality**
  - The parsing pipeline (`yaml_spanned` → `ConfigError` → `ToDiagnostics`) is cohesive and testable. This is a good foundation to “lock down”.

- **Environment semantics live in one place**
  - [env.rs](cci:7://file:///home/roman/dev/micromux/crates/micromux/src/env.rs:0:0-0:0) encapsulates dotenv parsing + single-pass expansion. That’s a clean, reusable unit.

- **Scheduler owns process lifecycle**
  - The scheduler uses PTYs and a consistent “emit events” model. Healthchecks integrate through the same event stream (nice uniformity).

## Main architectural risks / coupling issues (high value to fix *before* “locking down”)

### 1) **`micromux` public API surface is not intentionally designed**
Right now [micromux/src/lib.rs](cci:7://file:///home/roman/dev/micromux/crates/micromux/src/lib.rs:0:0-0:0) exports many modules (`pub mod ...`) and re-exports `CancellationToken`. This makes it hard to know what is “stable” vs “internal detail”, and it increases refactor cost later.

- **Why it matters**
  - If you “lock down” now, you’ll lock down accidental APIs.
  - Tests and other crates may keep reaching into internals because it’s convenient.

### 2) **[scheduler.rs](cci:7://file:///home/roman/dev/micromux/crates/micromux/src/scheduler.rs:0:0-0:0) is a monolith**
[scheduler.rs](cci:7://file:///home/roman/dev/micromux/crates/micromux/src/scheduler.rs:0:0-0:0) currently owns:
- event/command types
- dependency evaluation logic
- restart/disable policy logic
- PTY spawning + IO threads
- shutdown/termination sequencing
- event loop coordination

- **Why it matters**
  - This is already the “complexity hotspot”.
  - It will become the main brake on maintainability and adding new features (JSON output mode, multiple UIs, different runners, etc.).
  - Testing gets harder because logic isn’t separated into testable units.

### 3) **Mixed “config model” and “runtime behavior”**
Two examples:
- `impl crate::config::HealthCheck { run_loop / run }` attaches runtime behavior directly to a config struct.
- [service::Service](cci:2://file:///home/roman/dev/micromux/crates/micromux/src/config/mod.rs:147:0-158:1) contains runtime-ish fields (`state`, `health`, `process`) but the scheduler also maintains *separate* state (`service_state: HashMap<ServiceID, State>`), and PTY process lifecycle is largely handled outside of [Service](cci:2://file:///home/roman/dev/micromux/crates/micromux/src/config/mod.rs:147:0-158:1).

- **Why it matters**
  - It blurs ownership. It’s not obvious what the canonical state is.
  - It makes it harder to eventually support alternative execution backends (non-PTY, remote execution, etc.).
  - It’s easy to regress invariants because “the truth” is duplicated.

### 4) **TUI depends on strong invariants but encodes them with `unwrap()`**
The TUI frequently does `get_mut(...).unwrap()` for service lookups. That’s a correctness risk (and will fight clippy rules), and it couples the UI to assumptions that aren’t expressed in types.

- **Why it matters**
  - For “lock down”, you want invariants explicit:
    - either via APIs that guarantee existence
    - or via graceful handling of missing state (e.g. ignore events for unknown service)

## Are we ready to lock it down?

**Almost, but not yet.** The channel boundary (`Event`/`Command`) is strong enough to keep, but the *internal layering* inside `micromux` is still too “accidental”:
- public surface is too broad
- scheduler is too large
- config vs runtime boundaries are blurred

I’d strongly recommend doing **two “high leverage” refactors** before you declare the architecture stable.

# Proposed plan (high value, minimal churn, sets us up for lint/docs/tests)

## Phase 1 (must-do): define the *intended* core API surface (lock this down)
- **Goal**
  - Decide what `micromux` promises externally (to CLI/TUI/other future frontends).
- **Concrete changes**
  - **Keep public**:
    - [Micromux](cci:2://file:///home/roman/dev/micromux/crates/micromux/src/lib.rs:19:0-22:1) (or rename to `Engine` / `Runtime`)
    - `CancellationToken` (or accept it as dependency boundary)
    - `scheduler::{Event, Command, ServiceID}` (or move to `engine::protocol`)
    - `config::{from_str, find_config_file, ConfigFile, ConfigError}` (if you want core to own config parsing)
  - **Make internal (`pub(crate)`)**:
    - low-level scheduler helpers, PTY internals, log parsing helpers
    - possibly `env` (unless you consider it a supported public API)
- **Payoff**
  - Lets you refactor internals freely later.
  - Prevents the CLI/TUI from “reaching into internals” during future feature work.

## Phase 2 (must-do): split scheduler into modules (no behavior change)
- **Target structure (example)**
  - `scheduler/mod.rs` (main loop orchestration)
  - `scheduler/types.rs` (`Event`, `Command`, [State](cci:2://file:///home/roman/dev/micromux/crates/micromux-tui/src/state.rs:61:0-65:1), `OutputStream`, `LogUpdateKind`)
  - `scheduler/pty.rs` (spawn PTY process, log streaming, input writing, resizing)
  - `scheduler/deps.rs` (dependency satisfaction evaluation)
  - `scheduler/restart.rs` (backoff + restart policy)
- **Payoff**
  - Adds testable seams.
  - Makes correctness work (timeouts, termination, races) much easier to reason about.

## Phase 3 (high-value, medium scope): clarify config vs runtime ownership
Pick one of these directions:

- **Option A (least invasive)**: keep [Service](cci:2://file:///home/roman/dev/micromux/crates/micromux/src/config/mod.rs:147:0-158:1) mostly as “resolved config”, and keep runtime state *only* in scheduler-owned structs.
  - Remove/avoid fields in [Service](cci:2://file:///home/roman/dev/micromux/crates/micromux/src/config/mod.rs:147:0-158:1) that look runtime-owned (`state`, `health`, `process`) unless they’re actually used and canonical.
  - Move healthcheck execution into a runtime module (e.g. `healthcheck_runner`) rather than an `impl` on the config type.

- **Option B (more explicit)**: introduce `ResolvedService` + `RunningService` types.
  - `ResolvedService`: immutable, resolved paths/env/ports/command/working_dir.
  - `RunningService`: has handles (pty, killer, pgid, termination token, etc.) and current state.

**Payoff**
- Removes duplicated “truth”.
- Makes alternate UIs and future non-TUI outputs easier.

## Phase 4 (cleanup-friendly): small TUI hardening pass
- Replace `unwrap()`-based lookups with:
  - `if let Some(service) = ... { ... }` or
  - a small accessor that returns `Result<&mut Service, eyre::Report>` without unwrap/expect.
- Add a single translation point from `scheduler::Event` → `tui::state` updates (a “reducer”), so rendering code doesn’t also become state logic.

# Recommendation on “locking down”
If you do **Phase 1 + Phase 2**, I’d say you’re in a good place to “lock down” the architecture and then proceed to the final pass (lint/docs/tests). Phase 3 is high-value but can be staged if you want to minimize churn; however, I’d at least decide which direction you want *now*, because it affects how you split scheduler and what you expose publicly.

---

If you want, I can take the lead implementing **Phase 1 (API boundary)** first, since it’s the smallest change with the highest long-term payoff.

# Previous TODOs

- smoothly handle transitions between desired and actual state.

- switch between services and log view sections? 
    - for command help and key shortcuts, space for switching?

- we should have a fully independent state for the TUI with functions
    - this way, there is full decoupling and event-driven design

- implement desired vs actual state machine in the start service as a loop
- use a proper TTY for color detection, using crate such as portable-pty...
- allow showing the health checks
- fix the scrollbar
- shellexpand
- interpolate variable values with env variables
- parse dotfiles
- vim style search

- DONE: support colors
- DONE: figure out a debounced refreshing...
- DONE: start the processes
- DONE: use yaml spanned
- DONE: add logging to log file
