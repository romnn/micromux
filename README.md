# micromux
 
[<img alt="build status" src="https://img.shields.io/github/actions/workflow/status/romnn/micromux/build.yaml?branch=main&label=build">](https://github.com/romnn/micromux/actions/workflows/build.yaml)
[<img alt="test status" src="https://img.shields.io/github/actions/workflow/status/romnn/micromux/test.yaml?branch=main&label=test">](https://github.com/romnn/micromux/actions/workflows/test.yaml)
[![dependency status](https://deps.rs/repo/github/romnn/micromux/status.svg)](https://deps.rs/repo/github/romnn/micromux)
[<img alt="docs.rs" src="https://img.shields.io/docsrs/micromux/latest?label=docs.rs">](https://docs.rs/micromux)
[<img alt="crates.io" src="https://img.shields.io/crates/v/micromux">](https://crates.io/crates/micromux)

 Micromux is a **local process supervisor** with a **terminal UI**.

 Think of it as **Docker Compose for local processes** (not containers) — like **preconfigured tmux panes** that come with Compose-style dependencies, healthchecks, and restarts.

 <p align="center">
   <img src="https://raw.githubusercontent.com/romnn/micromux/main/docs/overview.png" alt="micromux TUI" width="900" />
 </p>

 It runs multiple long-lived commands (your dev “services”) on your machine, tracks their state, and gives you a single place to:
 - **see logs** (ANSI/interactive output supported)
 - **restart/disable services**
 - **gate startup by dependencies + healthchecks**
 - **send input to a service** (PTY-backed “attach” mode)

 <p align="center">
   <img src="https://raw.githubusercontent.com/romnn/micromux/main/docs/healthcheck.png" alt="micromux healthcheck pane showing a failed probe" width="820" />
   <br />
   <sub>Per-service healthcheck pane — a failing probe with its command and output.</sub>
 </p>

 <p align="center">
   <img src="https://raw.githubusercontent.com/romnn/micromux/main/docs/disable.png" alt="micromux with a service disabled" width="820" />
   <br />
   <sub>Disable a service on the fly — it stops and its row turns gray.</sub>
 </p>

 
 ## Install
 
 ```bash
 brew install --cask romnn/tap/micromux

 # Or install from source
 cargo install --locked micromux-cli
 ```
 
 ## Use
 
 Micromux looks for a config in the current directory:
 - `micromux.yaml`
 - `.micromux.yaml`
 - `micromux.yml`
 - `.micromux.yml`
 
 Start it:
 
 ```bash
 micromux
 ```
 
 Or specify a config explicitly:
 
 ```bash
 micromux --config ./micromux.yaml
 ```
 
 Minimal config example:
 
 ```yaml
 # yaml-language-server: $schema=https://github.com/romnn/micromux/raw/main/micromux.schema.json
version: "1"
restart: unless-stopped
healthcheck:
  interval: "2s"
  timeout: "1s"
  retries: 10
services:
  api:
    command: ["sh", "-c", "./run-api"]
    env_file: ".env"
    healthcheck:
      test: ["CMD-SHELL", "curl -fsS http://localhost:8080/health || exit 1"]

  worker:
    command: "./run-worker"
    disabled: true
    depends_on:
      - name: api
        condition: healthy
 ```

Set `disabled: true` on a service to leave it disabled when the session starts. Enable it later
from the TUI or with the control plane.
 
 TUI controls:
 - **Navigate**: `j`/`k` (or arrows)
 - **Restart**: `r` (current), `R` (all)
 - **Disable/enable**: `d`
 - **Attach mode (send input)**: `a` (exit attach mode with `Alt+Esc`)
 - **Toggle panes/focus**: `Tab`, healthchecks pane: `H`
 - **Logs**: wrap `w`, follow-tail `t`
 - **Quit**: `q` (or `Esc`)
 
 ## Agent control (MCP)

Micromux exposes an **MCP server** so coding agents (Claude Code, Codex) can discover and control your running sessions — list services, read logs, restart/enable/disable them, check health, and wait for a service to become healthy. Actions go through the **same control plane the TUI uses**, so dependency gating, healthchecks, and restart policy are respected — restarting a service via micromux is *more correct* than `kill` + rerun.

Every running `micromux` opens a local, per-project control endpoint (a Unix domain socket under `$XDG_RUNTIME_DIR/micromux/`, same-user only, no network). The MCP server is a thin stdio proxy. Configure it once, like `playwright-mcp`:

**Claude Code** (`.mcp.json`, or `claude mcp add micromux -- micromux mcp`):

```json
{
  "mcpServers": {
    "micromux": { "command": "micromux", "args": ["mcp"] }
  }
}
```

**Codex** (`~/.codex/config.toml`):

```toml
[mcp_servers.micromux]
command = "micromux"
args = ["mcp"]
```

Launched in a project directory, the tools target that project's session automatically. Target another with a `session` argument (`name:<n>`, `pid:<n>`, or `hash:<h>`) or the `MICROMUX_SESSION` env var. Tools include session and service discovery, config validation, lifecycle-event and log inspection, health diagnosis/waits, ordinary mutations, `ensure_service_ready`, session start/stop, and the `start_dynamic_service`/`replace_dynamic_service`/`stop_dynamic_service` runtime-service lifecycle. `restart_service`/`enable_service` return a run **generation**; pass it to `wait_for_healthy(after_generation=…)` to wait for the *new* run, not the old one. Manual restarts, enable, and due automatic restarts reload the latest `micromux.yaml` service definitions before spawning, so command flags, environment, ports, restart policy, healthcheck, and log-retention edits take effect without stopping the whole session.

`start_session` spawns a detached, headless `micromux serve` for a project, and `stop_session` stops a session and frees its ports — handy when switching between git worktrees that bind the same ports. `list_sessions` always includes discovery diagnostics, including sockets that exist but cannot answer because they are busy or speak another protocol version. `list_services` includes each service's resolved command (argv) and working directory, and its result carries a copy-pasteable `session_selector` (`hash:<id>`). `find_service` locates a service by id or name across every running session — returning each match's `session_selector`, config path, working dir, and current status — so you can retarget without the list_sessions → pick a hash → list_services dance; service-scoped tools also point at the sibling sessions that have the service when it is unknown in the selected one. `get_logs`/`follow_logs`/`follow_all_logs` strip ANSI color by default (`raw=true` keeps it), return logical log entries instead of wrapped terminal rows, trim terminal padding, and accept `grep`, `grep_context`, `since`/`since_unix_ms`, and `trace_id` filters; for services that emit JSON logs, `min_level` (`trace`…`fatal`) filters by structured level and each entry carries its detected `level`, micromux ingestion timestamp, parsed JSON source timestamp, `message`, and typed JSON `fields`. Use `format: "compact"` to return token-efficient lines like timestamp + level + message + key/value fields instead of the raw JSON string. Call `log_cursors` before an action, then pass its cursor map to `follow_all_logs(after=…)` or `wait_for_log(service="*", after=…)` to inspect the resulting logs across services with a timestamp-guided merge that preserves each service's cursor order. Cursor `0` means "before the first entry" for a service with no logs yet. `diagnose` returns a one-shot summary of exited or unhealthy services with their state, current live-run healthcheck output when applicable, and compact likely-cause log lines. On a `wait_for_healthy` timeout the response includes the execution sub-state and current live-run healthcheck output when applicable, so "still starting" is distinguishable from "process up, probe failing".

Name a session so agents can find it by name:

```yaml
name: my-project
```

Use `strict: true` to treat config warnings as errors for the whole project.

Structured JSON service logs stay raw for MCP/control tooling, but the TUI renders them as compact
colored log lines by default. Opt out per run with `--no-pretty-json-logs`, or for the whole team:

```yaml
ui:
  pretty_json_logs: false
```

Retain full disk-backed logs for recent runs so agents can inspect crash output after restarts.
The in-memory TUI/default log stream stays bounded and fast; disk run logs are unbounded and rotate
by run count. `get_logs` returns a bounded tail; use `follow_logs` with a retained
`run_generation` and `next_seq` to page through larger run logs. Service-level `logs` overrides
inherit unspecified fields from the global block:

```yaml
logs:
  retained_runs: 5
  memory:
    max_lines: 1000
    max_bytes: 67108864

services:
  api:
    command: ["sh", "-c", "./run-api"]
    logs:
      retained_runs: 10
      memory:
        max_lines: unbounded
```

`restart` and healthcheck timing (`start_delay`, `interval`, `timeout`, `retries`) can also be
set globally and overridden per service. A global `healthcheck` block only supplies timing defaults;
each service still opts in by defining `healthcheck.test`.

The control plane is **on by default**; opt out with `--no-control` or `control: { enabled: false }`. Dogfood it from the shell without an agent:

```bash
micromux ctl ls
micromux ctl log-runs api
micromux ctl logs api --tail 50
micromux ctl logs api --run-generation 2 --tail 200
micromux ctl restart api
```

Runtime-created services are disabled by default. Enable and bound them in the target session's
config; policy changes are latched when the session starts and require a restart:

```yaml
control:
  dynamic_services:
    enabled: true
    allowed_working_roots: ["."]
    max_services: 4
    max_lifetime: 12h
```

Dynamic services are bounded to the configured lifetime by default and always stop with the session.
Set `max_lifetime: none` to explicitly opt in to session-lifetime leases; callers can then request
`expires_after: none`. `renew_dynamic_service` extends or removes a live service's deadline without
restarting it. This policy limits accidental blast radius for a same-user local control tool; it is
not a security sandbox, because commands still run as the session user. Environment values can be
sent inward or cloned server-side with `from_service`; snapshots omit the environment entirely, and
receipts return only key names.

Use `validate_config` to check a candidate config without a running session. For edits to the active
session's on-disk config, run `reconcile_config` as a dry run first, then apply it; additions and
removals take effect immediately, while changed definitions are used on the next restart. Terminal
attachment is deliberately not part of this control surface yet; its design is tracked in the
[attach plan](plan/attach.md).

Build a lean TUI-only binary with the MCP server compiled out via `cargo install --no-default-features micromux-cli`.

**Platform support.** micromux is developed and fully supported on Linux and macOS. On Windows the
TUI runs, but the agent control plane (`micromux ctl`, `micromux mcp`, `micromux serve`) is not yet
available, and stopping a service kills it immediately without a graceful-termination phase. Windows
named-pipe support is planned (`ControlEndpoint::WindowsNamedPipe` reserves the slot).

 ## How it differs from Docker Compose
 
 Micromux is **not a container orchestrator**.
 - **Runs host processes**: no images, builds, networks, volumes, or container isolation.
 - **Fast local workflow**: start/stop/restart your existing scripts/binaries with a UI.
 - **Dependency + health gating**: delay starting a service until deps are started/healthy/completed.
 
 If you need reproducible environments, networking, volumes, or cross-machine parity, use Docker Compose.
 
 ## How it differs from tmux/screen
 
 tmux/screen are **terminal multiplexers**.
 
 Micromux adds “service awareness”:
 - **Structured service lifecycle** (pending/starting/running/healthy/unhealthy/exited/disabled)
 - **Restart policies** (`always`, `unless-stopped`, `on-failure[:N]`)
 - **Healthchecks and dependency conditions**
 - **Single aggregated UI** for selecting services and viewing logs
