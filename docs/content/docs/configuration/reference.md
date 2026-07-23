---
title: Configuration reference
weight: 6
---

# Configuration reference

Every key in `micromux.yaml`. The machine-readable source of truth is [`micromux.schema.json`](https://github.com/romnn/micromux/blob/main/micromux.schema.json); reference it from your config for editor completion and validation:

```yaml
# yaml-language-server: $schema=https://github.com/romnn/micromux/raw/main/micromux.schema.json
```

## Top level

| Key | Type | Default | Description |
|---|---|---|---|
| `version` | string / number | — | Config format version. Use `"1"`. |
| `name` | string | directory name | Session name surfaced to agents via the control plane. |
| `strict` | bool | `false` | Treat config warnings as errors. |
| `services` | map | — | Service definitions, keyed by id. |
| `restart` | string | — | Default [restart policy]({{< relref "restart-policies.md" >}}). |
| `healthcheck` | object | — | Default [healthcheck timing]({{< relref "healthchecks.md" >}}) (no `test`). |
| `logs` | object | — | Default [log retention]({{< relref "logs.md" >}}). |
| `ui` | object | — | Terminal-UI options. |
| `control` | object | — | [Control plane]({{< relref "../agent-control/_index.md" >}}) and dynamic-service policy. |

## `services.<id>`

| Key | Type | Description |
|---|---|---|
| `command` | string / array | **Required.** Shell-like string or argv array. |
| `name` | string | Display name for the TUI. |
| `disabled` | bool | Leave the service disabled when the session starts. |
| `working_dir` | string | Working directory, relative to the config. Aliases: `cwd`, `directory`. |
| `environment` | map | Inline environment variables. |
| `env_file` | string / object / array | `.env` file(s) to load. Long form is `{ path: … }`. |
| `depends_on` | array | [Dependencies]({{< relref "dependencies.md" >}}); each a service id or `{ name, condition }`. |
| `healthcheck` | object | A [probe]({{< relref "healthchecks.md" >}}) with `test` plus timing. |
| `ports` | array | Ports the service uses (metadata; not bound by micromux). |
| `restart` | string | [Restart policy]({{< relref "restart-policies.md" >}}) for this service. |
| `logs` | object | [Log retention]({{< relref "logs.md" >}}) for this service. |
| `color` | bool | Force color handling for this service. |

## `depends_on[]`

A bare string (the `started` condition) or an object:

| Key | Type | Description |
|---|---|---|
| `name` | string | **Required.** The dependency's service id. |
| `condition` | string | `started`, `healthy`, or `completed`. |

## `healthcheck`

| Key | Type | Description |
|---|---|---|
| `test` | string / array | The probe command. Required on a service; absent in the top-level defaults block. |
| `start_delay` | duration | Grace period before the first probe. Aliases: `startup_delay`, `initial_delay`. |
| `interval` | duration | Time between probes. |
| `timeout` | duration | Per-probe time limit. |
| `retries` | integer | Consecutive failures tolerated before **unhealthy**. |

## `restart`

One of `always`, `unless-stopped`, `on-failure` (or `on-failure:N`), `no` (synonym `never`). Case-insensitive; `-` and `_` are interchangeable.

## `logs`

| Key | Type | Default | Description |
|---|---|---|---|
| `retained_runs` | integer | `5` | Full disk-backed runs kept, including the current run. Aliases: `runs`, `history`. |
| `memory.max_lines` | integer / `unbounded` | — | In-memory tail line bound. |
| `memory.max_bytes` | integer / `unbounded` | — | In-memory tail byte bound. |
| `max_lines`, `max_bytes` | — | — | Shorthand for the `memory.*` fields. |

## `ui`

| Key | Type | Default | Description |
|---|---|---|---|
| `width` | integer | — | Initial sidebar width, in columns. |
| `pretty_json_logs` | bool | `true` | Render structured JSON logs as compact colored lines in the TUI. |

## `control`

| Key | Type | Default | Description |
|---|---|---|---|
| `enabled` | bool | `true` | Enable the local control endpoint. Changes require a session restart. |
| `dynamic_services.enabled` | bool | `false` | Permit [runtime-created services]({{< relref "../agent-control/dynamic-services.md" >}}). |
| `dynamic_services.allowed_working_roots` | array | `["."]` | Allowed working-dir roots, resolved relative to the config. |
| `dynamic_services.max_services` | integer | `4` | Maximum live dynamic services. |
| `dynamic_services.max_lifetime` | duration / `none` | `12h` | Default and maximum dynamic-service lifetime. |
