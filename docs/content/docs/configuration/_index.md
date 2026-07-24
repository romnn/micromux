---
title: Configuration
weight: 4
bookCollapseSection: true
---

# Configuration

micromux is configured by a single `micromux.yaml` (or `.micromux.yaml` / `.yml`) in your project directory. Its shape is deliberately close to Docker Compose, so `services`, `depends_on`, `healthcheck`, `restart`, `ports`, `environment`, and `env_file` mean what you'd expect.

```yaml
# yaml-language-server: $schema=https://github.com/romnn/micromux/raw/main/micromux.schema.json
version: "1"
name: my-project        # optional; surfaced to agents, defaults to the directory name
restart: unless-stopped # default policy, inherited by services
services:
  api:
    command: ["sh", "-c", "./run-api"]
    # …
```

Add the `$schema` comment at the top and an editor with the YAML language server gives you completion and validation against [`micromux.schema.json`]({{< relref "reference.md" >}}).

## Top-level keys

| Key | Purpose |
|---|---|
| `version` | Config format version. Use `"1"`. |
| `name` | Session name shown to agents via the control plane. Defaults to the working-directory name. |
| `strict` | Treat config warnings as errors. Also `--strict` / `MICROMUX_STRICT`. |
| `services` | The map of service definitions. See [Services]({{< relref "services.md" >}}). |
| `restart` | Default [restart policy]({{< relref "restart-policies.md" >}}) inherited by services. |
| `healthcheck` | Default [healthcheck timing]({{< relref "healthchecks.md" >}}) inherited by services (timing only — it never creates a probe). |
| `logs` | Default [log retention]({{< relref "logs.md" >}}) inherited by services. |
| `ui` | Terminal-UI options — see below. |
| `control` | The [agent control plane]({{< relref "../agent-control/_index.md" >}}) and runtime-service policy. |

## In this section

- **[Services]({{< relref "services.md" >}})** — `command`, working directory, environment, and ports.
- **[Dependencies & startup order]({{< relref "dependencies.md" >}})** — `depends_on` and its conditions.
- **[Healthchecks]({{< relref "healthchecks.md" >}})** — probes, timing, retries, and inherited defaults.
- **[Restart policies]({{< relref "restart-policies.md" >}})** — `always`, `unless-stopped`, `on-failure[:N]`, `no`.
- **[Logs]({{< relref "logs.md" >}})** — in-memory tail versus retained disk runs, and structured JSON.
- **[Configuration reference]({{< relref "reference.md" >}})** — every key, and the JSON schema.

## Inheritance

`restart`, `healthcheck` timing, and `logs` can be set once at the top level and overridden per service. A service that omits a field inherits the global default; a service that sets it wins for the fields it sets. This keeps common policy in one place while letting individual services differ.

## UI options

```yaml
ui:
  width: 40              # initial sidebar width, in columns
  pretty_json_logs: true # render structured JSON logs as compact colored lines (default)
```

Services that emit JSON logs are, by default, rendered in the TUI as compact colored log lines. Opt out per run with `--no-pretty-json-logs`, or for everyone with `ui: { pretty_json_logs: false }`. The raw JSON is always preserved for the control plane and MCP tools.
