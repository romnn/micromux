---
title: Logs
weight: 5
---

# Logs

micromux captures each service's output two ways at once:

- an **in-memory tail** — bounded and fast, backing the TUI and the default log stream;
- **disk-backed run logs** — the newest 64 MiB segment of recent runs, retained so you can inspect crash output *after* a restart.

```yaml
logs:
  retained_runs: 5        # bounded disk-backed runs kept, including the current one
  memory:
    max_lines: 1000
    max_bytes: 67108864   # 64 MiB

services:
  api:
    command: "./run-api"
    logs:
      retained_runs: 10   # overrides the global default for this service
```

## Retained runs

`retained_runs` (aliases: `runs`, `history`; default `5`) is how many service **runs** are kept on disk, counting the current run. Each restart starts a new run; when the count is exceeded the oldest run is dropped. This is what lets an agent read the logs of the run that crashed even though the service has since restarted. A run file rotates in place at 64 MiB, preserving its newest segment so a single long-lived or noisy process cannot consume disk without limit.

List the retained runs and read a specific one with the control plane:

```bash
micromux ctl log-runs api
micromux ctl logs api --run-generation 2 --tail 200
```

## In-memory tail

`memory.max_lines` and `memory.max_bytes` bound the in-memory tail that the TUI and default log stream use, so a chatty service can't grow memory without limit. Each accepts a positive integer or an explicit `unbounded` (synonyms: `unlimited`, `none`):

```yaml
logs:
  memory:
    max_lines: unbounded   # keep every line in memory
    max_bytes: 134217728
```

`logs.max_lines` and `logs.max_bytes` are accepted as shorthand for the nested `memory.*` form.

## Inheritance

Like `restart` and `healthcheck` timing, `logs` set at the top level is inherited by every service, and a service's `logs` block overrides only the fields it sets — the rest fall back to the global values.

## Structured JSON logs

Services that emit JSON logs are rendered in the TUI as compact, colored log lines by default, while the raw JSON is preserved for the control plane and MCP tools. Turn the pretty rendering off per run with `--no-pretty-json-logs`, or globally:

```yaml
ui:
  pretty_json_logs: false
```

Over [MCP]({{< relref "../agent-control/_index.md" >}}), JSON logs can be filtered by structured level (`min_level`) and returned in a token-efficient `compact` form; each entry carries its detected level, timestamps, message, and typed fields.
