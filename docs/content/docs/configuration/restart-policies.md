---
title: Restart policies
weight: 4
---

# Restart policies

A restart policy decides what micromux does when a service's process exits. Set it globally, per service, or both — a service inherits the top-level default and overrides it if it sets its own.

```yaml
restart: unless-stopped        # default for all services
services:
  api:
    command: "./run-api"       # inherits unless-stopped
  migrate:
    command: "./migrate"
    restart: "no"              # one-shot
  flaky:
    command: "./flaky"
    restart: on-failure:5      # up to 5 retries on non-zero exit
```

## Policies

| Policy | Behavior |
|---|---|
| `always` | Restart whenever the process exits, whatever the exit status. |
| `unless-stopped` | Restart on exit, **unless** you disabled/stopped the service yourself. |
| `on-failure` | Restart only on a non-zero exit. Add a cap with `on-failure:N` (e.g. `on-failure:5`). |
| `no` | Never restart. Use for one-shot jobs like migrations. (`never` is accepted as a synonym.) |

`on-failure` may be written with a count as `on-failure:3` or `on-failure=3`. Policy names are case-insensitive and accept `-` or `_` (`unless-stopped` or `unless_stopped`).

## Graceful stops

Micromux first asks a service to terminate gracefully, then force-kills it if it is still running after 10 seconds. Set `stop_grace_period` per service when it needs more or less time to shut down:

```yaml
services:
  database:
    command: "./run-database"
    stop_grace_period: 30s
```

The duration must be greater than zero and no longer than five minutes. Session shutdown waits for the longest grace period among the running services before applying its final cleanup budget.

## Manual restarts always win

The policies above govern **automatic** restarts. You can always restart a service yourself regardless of policy — with `r` in the TUI, `micromux ctl restart <id>`, or an agent over MCP — and a manual restart, an enable, and a due automatic restart all reload the latest `micromux.yaml` service definition before spawning. So edits to a service's command, environment, ports, restart policy, healthcheck, or log retention take effect on its next restart, without stopping the whole session. See [Reconciling config]({{< relref "../agent-control/control-plane.md" >}}#reconcile-on-disk-changes).
