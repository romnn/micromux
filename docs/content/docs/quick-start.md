---
title: Quick start
weight: 3
---

# Quick start

This walks through a first config, a first run, and how to read the UI. It assumes `micromux` is [installed]({{< relref "installation.md" >}}).

## 1. Write a config

micromux looks for one of `micromux.yaml`, `.micromux.yaml`, `micromux.yml`, or `.micromux.yml` in the current directory. Create one describing your services:

```yaml
# yaml-language-server: $schema=https://github.com/romnn/micromux/raw/main/micromux.schema.json
version: "1"
restart: unless-stopped
services:
  api:
    command: ["sh", "-c", "./run-api"]
    env_file: ".env"
    ports: [8080]
    healthcheck:
      test: ["CMD-SHELL", "curl -fsS http://localhost:8080/health || exit 1"]

  worker:
    command: "./run-worker"
    depends_on:
      - name: api
        condition: healthy
```

The `$schema` comment turns on completion and validation in editors with the YAML language server. Each service needs a `command`; everything else is optional. See [Configuration]({{< relref "configuration/_index.md" >}}) for the full set of fields.

## 2. Run it

From the directory containing the config:

```bash
micromux
```

Or point at a config explicitly:

```bash
micromux --config ./micromux.yaml
```

micromux starts every enabled service, honoring `depends_on` — here `worker` waits until `api` reports **healthy** — and opens the TUI.

## 3. Read the UI

The sidebar lists your services; each row shows its lifecycle state. Selecting a service shows its live log output in the main pane.

{{< figure src="images/overview.png" alt="The micromux sidebar with services in different states next to a log pane" caption="Each row carries a service's state: healthy, running, exited (completed), unhealthy, or pending." >}}

Move with `j`/`k` (or the arrow keys), restart the selected service with `r`, and toggle it disabled with `d`. When a healthcheck is failing, open the healthcheck pane with `H` to see the probe command and its output:

{{< figure src="images/healthcheck.png" alt="The healthcheck pane showing a failed probe, its command, and its output" caption="The healthcheck pane: a failing probe with its command and captured output." >}}

The full set of keys and panes is in [The terminal UI]({{< relref "tui.md" >}}).

## 4. Steer from the shell

Every running session also exposes a local control plane, so you can inspect and steer it without touching the TUI — handy in a second terminal or a script:

{{< terminal name="services" >}}

```bash
micromux ctl restart api      # restart a service (respecting dependencies + health)
micromux ctl logs api --tail 50
micromux ctl health payments  # why is a probe failing?
```

This is the same control plane coding agents use over [MCP]({{< relref "agent-control/_index.md" >}}).

## 5. Leave a service off at start

Set `disabled: true` to keep a service disabled when the session starts; enable it later from the TUI (`d`) or with `micromux ctl enable <service>`:

```yaml
services:
  worker:
    command: "./run-worker"
    disabled: true
```

## Next steps

- [Configuration]({{< relref "configuration/_index.md" >}}) — dependencies, healthchecks, restart policies, and logs in depth.
- [The terminal UI]({{< relref "tui.md" >}}) — every key and pane, plus attaching to a running session.
- [Agent control]({{< relref "agent-control/_index.md" >}}) — let Claude Code or Codex drive the stack.
