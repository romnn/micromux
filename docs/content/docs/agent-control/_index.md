---
title: Agent control
weight: 6
bookCollapseSection: true
---

# Agent control

micromux exposes an **MCP server** so coding agents — Claude Code, Codex — can discover and control your running sessions: list services, read logs, restart, enable, and disable them, check health, and wait for a service to become healthy. Every action goes through the **same control plane the TUI uses**, so dependency gating, healthchecks, and restart policy are respected — restarting a service via micromux is *more correct* than `kill` + rerun.

## How it fits together

Every running `micromux` opens a local, per-project **control endpoint** — a Unix domain socket under `$XDG_RUNTIME_DIR/micromux/`, same-user only, no network. Three clients speak to it:

- the **TUI**, on your keypresses;
- the **`micromux ctl`** command, for the shell — see [The control plane]({{< relref "control-plane.md" >}});
- the **MCP server** (`micromux mcp`), a thin stdio proxy for agents.

The control plane is **on by default**; opt out with `--no-control` or `control: { enabled: false }`.

## Configure the MCP server

Configure it once, like `playwright-mcp`.

**Claude Code** — `.mcp.json`, or `claude mcp add micromux -- micromux mcp`:

```json
{
  "mcpServers": {
    "micromux": { "command": "micromux", "args": ["mcp"] }
  }
}
```

**Codex** — `~/.codex/config.toml`:

```toml
[mcp_servers.micromux]
command = "micromux"
args = ["mcp"]
```

Launched in a project directory, the tools target that project's session automatically. Target another with a `session` argument (`name:<n>`, `pid:<n>`, or `hash:<h>`) or the `MICROMUX_SESSION` environment variable.

Give a session a name so agents can find it by name:

```yaml
name: my-project
```

## What agents can do

The tools cover the full lifecycle:

- **Discovery** — `list_sessions`, `list_services`, `find_service` (locate a service across every running session).
- **Logs** — `get_logs`, `follow_logs`, `follow_all_logs`, `list_log_runs`, with `grep`, time, trace-id, and — for JSON logs — structured `min_level` filters and a token-efficient `compact` format.
- **Health & forensics** — `get_health`, `get_health_history`, `wait_for_healthy`, `get_service_events`, `diagnose` (a one-shot summary of exited or unhealthy services with likely-cause log lines).
- **Mutations** — `restart_service`, `enable_service`, `disable_service`, `restart_all`; `restart_service`/`enable_service` return a run **generation** to pass to `wait_for_healthy(after_generation=…)` so you wait for the *new* run, not the old one.
- **Session lifecycle** — when the MCP server has `--allow-session-start` (add it to the configuration above if wanted), `start_session` spawns a detached headless `micromux serve`, capped at eight requests per minute; `stop_session` stops a session and frees its ports (handy when switching between git worktrees that bind the same ports).
- **Config** — `validate_config` (a candidate file) and `reconcile_config` (apply on-disk edits to a live session; see [Reconcile]({{< relref "control-plane.md" >}}#reconcile-on-disk-changes)).
- **Runtime services** — the `start_dynamic_service` / `replace_dynamic_service` / `stop_dynamic_service` lifecycle. See [Dynamic services]({{< relref "dynamic-services.md" >}}).

Manual restarts, enables, and due automatic restarts reload the latest `micromux.yaml` service definitions before spawning, so command, environment, port, restart-policy, healthcheck, and log-retention edits take effect without stopping the session.

> [!NOTE]
> MCP deliberately exposes **no service-PTY-input tool** — human observation of a headless session goes through [`micromux attach`]({{< relref "../tui.md" >}}#attach-to-a-running-session). For a lean, TUI-only binary with the MCP server compiled out, build with `cargo install --no-default-features micromux-cli`.

> [!WARNING]
> The same-user control interface is operator-trusted, not a redacted security boundary. Discovery
> reports config and working-directory paths, executable/runtime endpoint details, service and
> healthcheck argv, and captured probe output. Environment values are excluded, but secrets placed
> in argv or output are visible to connected clients.

## In this section

- **[The control plane]({{< relref "control-plane.md" >}})** — the `micromux ctl` client, `serve`, and reconciling on-disk edits.
- **[Dynamic services]({{< relref "dynamic-services.md" >}})** — runtime-created services and the policy that bounds them.
