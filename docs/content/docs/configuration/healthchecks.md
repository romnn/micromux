---
title: Healthchecks
weight: 3
---

# Healthchecks

A healthcheck is a command micromux runs periodically to decide whether a service is **healthy**. Health drives two things: how the service is shown in the UI, and whether `depends_on … condition: healthy` gates are satisfied.

```yaml
services:
  api:
    command: "./run-api"
    healthcheck:
      test: ["CMD-SHELL", "curl -fsS http://localhost:8080/health || exit 1"]
      start_delay: "2s"
      interval: "10s"
      timeout: "2s"
      retries: 3
```

## The probe

`test` is the command to run, in the same string-or-array form as a service [`command`]({{< relref "services.md" >}}). The Compose-style `CMD-SHELL` prefix runs the rest through a shell:

```yaml
test: ["CMD-SHELL", "curl -fsS http://localhost:8080/health || exit 1"]
test: "pg_isready -q"        # a plain string works too
```

A probe that exits `0` is a pass; any non-zero exit (or a timeout) is a failure.

## Timing

| Field | Meaning |
|---|---|
| `start_delay` | Grace period after the process starts before the first probe runs. Aliases: `startup_delay`, `initial_delay`. |
| `interval` | Time between probes. |
| `timeout` | How long a single probe may run before it counts as a failure. |
| `retries` | Consecutive failures tolerated before the service is marked **unhealthy**. `0` marks it unhealthy on the first failure. |

Durations are strings like `"500ms"`, `"2s"`, `"1m"`.

## Inherited defaults

Set common timing once at the top level and every service with a healthcheck inherits it, overriding only what it needs:

```yaml
healthcheck:            # timing defaults for all services (no `test` here)
  interval: "10s"
  timeout: "2s"
  retries: 3

services:
  api:
    command: "./run-api"
    healthcheck:
      test: ["CMD-SHELL", "curl -fsS localhost:8080/health || exit 1"]  # inherits the timing
  db:
    command: "postgres -D ./pgdata"
    healthcheck:
      test: "pg_isready -q"
      interval: "5s"       # overrides just the interval
```

> [!NOTE]
> A top-level `healthcheck` block supplies **timing only** — it never defines a probe. Each service still opts in by giving its own `healthcheck.test`. A service with no `test` has no health.

## Inspecting a failing probe

When a probe is failing, the [healthcheck pane]({{< relref "../tui.md" >}}) (`H`) shows the last attempt — its command, exit status, and output:

{{< terminal name="health" >}}

From the shell, `micromux ctl health <id>` prints the same, and `--history` shows the retained attempts rather than only the latest.
