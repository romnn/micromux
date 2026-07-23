---
title: Dependencies & startup order
weight: 2
---

# Dependencies & startup order

`depends_on` gates a service's startup on the state of other services. This is the difference between a wall of panes that all launch at once and a stack that comes up in the right order.

```yaml
services:
  api:
    command: "./run-api"
    depends_on:
      - name: postgres
        condition: healthy
      - name: migrate
        condition: completed

  postgres:
    command: "postgres -D ./pgdata"
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -q"]

  migrate:
    command: "./migrate"
    restart: "no"
```

Here `api` starts only once `postgres` is **healthy** and `migrate` has **completed** successfully.

## Conditions

Each dependency is either a bare service id or an object with a `condition`:

| Condition | Waits until the dependency… | Requires |
|---|---|---|
| `started` | has been started (process spawned) | — |
| `healthy` | reports healthy | a [`healthcheck`]({{< relref "healthchecks.md" >}}) on the dependency |
| `completed` | exited **successfully** (status 0) | a service that terminates |

A bare string is shorthand for the `started` condition:

```yaml
depends_on:
  - postgres          # same as { name: postgres, condition: started }
  - name: redis
    condition: healthy
```

> [!NOTE]
> The `healthy` condition only makes sense if the dependency defines a `healthcheck` — without one, a service has no health to report. Likewise `completed` is for one-shot services (migrations, seeders) that exit, not long-lived ones.

## How gating shows up

A service waiting on its dependencies sits in the **pending** state until they are satisfied, then transitions to starting. In the TUI a pending service is visibly distinct, so a stack that's mid-startup — or stuck behind an unhealthy dependency — is obvious at a glance.

If a dependency never becomes healthy (its probe keeps failing), the dependent stays pending. Open the [healthcheck pane]({{< relref "../tui.md" >}}) (`H`) on the dependency, or run `micromux ctl health <id>`, to see why.

## Restarts respect dependencies

Because restarts go through the [control plane]({{< relref "../agent-control/control-plane.md" >}}), restarting a service — from the TUI, `micromux ctl`, or an agent — re-applies its gating. This is why restarting *through* micromux is more correct than killing and re-running a process by hand.
