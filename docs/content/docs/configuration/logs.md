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

## Viewing structured logs

Let a service log verbosely and decide in the TUI how much of it to see. `level` sets the initial level threshold, `timestamps` whether lines lead with their local time, `fields` which field keys to leave out of the display, and `filter_fields` whether they start out left out. Top-level values are defaults for every service:

```yaml
logs:
  level: debug            # all, debug, info, warn, or error; trace means all
  timestamps: true        # the default
  filter_fields: true     # the default; false starts with every field shown
  fields: {filename: hide, line_number: hide, span: hide, spans: hide}

services:
  api:
    command: "RUST_LOG=trace ./run-api"
    logs:
      timestamps: false   # level, timestamps, and filter_fields replace the default
      fields:             # fields merge into the default key by key
        span: show        # undo an inherited hide
        latency: hide     # hide one more
```

Lines without a structured level, such as build output and panic messages, are never hidden. `fields` keys match top-level keys and the keys of a tracing-style nested `fields` object exactly. In the TUI, `L` changes the threshold, `T` toggles the timestamps, and `F` shows the hidden fields again, each for the selected service. A config reload applies new `logs` display settings without restarting the service.

These settings affect only the TUI. Agents reading logs always get every record, field, and timestamp.

Over [MCP]({{< relref "../agent-control/_index.md" >}}), JSON logs can be filtered by structured level (`min_level`) and returned in a token-efficient `compact` form; each entry carries its detected level, timestamps, message, and typed fields.
