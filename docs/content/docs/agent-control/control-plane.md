---
title: The control plane
weight: 1
---

# The control plane

The control plane is the local endpoint every running `micromux` opens. The TUI, the MCP server, and the `micromux ctl` client all speak to it, so any of them can inspect and steer a session with identical semantics.

## `micromux ctl`

`ctl` is the shell client — the same control plane an agent uses, dogfooded from your terminal. It targets the session for the config resolved from the current directory (or one you name with `--session`).

{{< terminal name="services" >}}

Common actions:

```bash
micromux ctl ls                       # list services and their state
micromux ctl logs api --tail 50       # recent logs for a service
micromux ctl log-runs api             # retained run generations
micromux ctl logs api --run-generation 2 --tail 200
micromux ctl restart api              # restart (respecting deps + health)
micromux ctl restart-all
micromux ctl enable worker            # enable (and start)
micromux ctl disable worker
micromux ctl health payments          # latest healthcheck attempt (--history for all)
micromux ctl describe                 # session identity
micromux ctl stop                     # stop the whole session, freeing its ports
```

Because these go through the control plane, a restart re-applies dependency gating and reloads the latest service definition — the same reason restarting *through* micromux beats `kill` + rerun.

### Inspecting health

`micromux ctl health <id>` prints the latest probe for a service's live run — its command, exit status, and output:

{{< terminal name="health" >}}

Add `--history` to see the retained attempts (oldest first) instead of only the latest — useful when a probe is flapping.

## Headless sessions: `micromux serve`

`micromux serve` runs the supervisor **without a TUI**, serving the control plane until stopped. It's how agent-managed sessions run: an agent calls the MCP `start_session` tool (which spawns a detached `serve`), works against it, and you can watch with [`micromux attach`]({{< relref "../tui.md" >}}#attach-to-a-running-session) or steer it with `micromux ctl`.

```bash
micromux serve                    # headless; control plane only
micromux serve --config ./micromux.yaml
```

Stop a headless session with `micromux ctl stop` or the MCP `stop_session` tool.

## Reconcile on-disk changes

When you edit a live session's `micromux.yaml`, apply the changes without restarting the whole session:

```bash
micromux ctl reconcile --dry-run   # show the semantic diff
micromux ctl reconcile             # apply it
```

Reconciliation **adds** newly-defined services, **retires** removed ones, and **updates** changed definitions. Additions and removals take effect immediately; a changed definition is used on the service's **next restart** (manual, enable, or a due automatic restart). Reconciliation does not by itself restart a changed process.

> [!NOTE]
> Over MCP the same flow is `reconcile_config` — run it with `dry_run=true` first, then apply. For a config that has no running session, `validate_config` checks a candidate file without starting anything.

## Protocol compatibility

Protocol 3 peers accept additive fields from newer minor revisions. Revision 3.8 changed transient disk-log rotation and reader saturation failures from `LimitExceeded` to the retryable `Busy` code. Revision 3.9 distinguishes an uninitialized disk-reader pool and reports reads that still occupy workers after their callers leave.
