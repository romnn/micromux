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
micromux ctl wait api                 # block until api is healthy (or fail fast)
micromux ctl wait migrate --exit      # block until a one-shot run exits
micromux ctl describe                 # session identity, browser URLs, allocated ports
micromux ctl stop                     # stop the whole session, freeing its ports
```

Because these go through the control plane, a restart re-applies dependency gating and reloads the latest service definition — the same reason restarting *through* micromux beats `kill` + rerun.

### Inspecting health

`micromux ctl health <id>` prints the latest probe for a service's live run — its command, exit status, and output:

{{< terminal name="health" >}}

Add `--history` to see the retained attempts (oldest first) instead of only the latest — useful when a probe is flapping.

### Waiting from a script

`micromux ctl wait <id>` blocks until a service is up, and healthy if it has a healthcheck. With `--exit` it waits for the service's run to exit instead, for one-shot jobs such as migrations. It gives up after `--timeout` (default `60s`), and `--after-generation <n>` accepts only a run newer than generation `n`, such as the one `ctl restart` printed.

A wait does not hang on something that cannot happen. It fails at once when the service is disabled or retired, or when it is blocked behind a dependency that will not meet its condition without your help: one that exited and whose restart policy will not start it again, that is disabled or retired, or that is waited on as `healthy` but has no healthcheck, directly or further down the chain. The message names that dependency, its state, and its exit code.

The exit code tells the outcomes apart:

| Code | Meaning |
|---|---|
| `0` | The service is healthy, or the job completed; with `--exit`, its run exited with status 0. |
| `3` | The run exited instead, or the job failed; with `--exit`, it exited with a non-zero status. |
| `4` | The service is blocked behind a dependency that failed. |
| `5` | The service is disabled or retired, or the job finished or is a manual job that has not run, and nothing asked it to run again. |
| `124` | The timeout elapsed. |

After a restart, pass the generation `ctl restart` printed, so the wait cannot settle on the run the restart replaced:

```bash
generation=$(micromux ctl restart seed | sed -n 's/.*(generation \([0-9]*\)).*/\1/p')
micromux ctl wait seed --exit --after-generation "$generation" && micromux ctl wait api
```

The MCP `wait_for_healthy` and `wait_for_exit` tools follow the same rules; a dependency failure comes back as an error whose data names the failed dependency.

## Run commands in the session environment

A task you run yourself, such as a migration, a seed script, or a one-off `psql`, needs the ports and variables the session's services get. `micromux exec` runs it with them, and `micromux env` prints them:

```bash
micromux exec task migrations:reset                   # run with the session's variables
micromux exec --service api -- cargo run --bin seed   # run with what the `api` service gets
micromux env                                          # KEY=value lines, quoted for a shell
psql -p "$(micromux env POSTGRES_PORT)"               # one name prints just its value
eval "$(micromux env --export)"                       # load them into the current shell
micromux env --service api --format json
```

Both read the environment from the running session over the control plane, so they match what its services got, and `exec` and `env` never disagree. Without a running session they fail instead of falling back to `.env` files. On success they print nothing besides the requested output.

**Which session.** Like `ctl`, they use the session for the config found at or above the current directory, or for `--config <path>`. A session for any other config does not count, not even one in a subdirectory. `--session` selects a session by `name:`, `pid:`, or `hash:`, as [`micromux attach`]({{< relref "../tui.md" >}}#attach-to-a-running-session) does.

**Which variables.**

- Without `--service`, the variables the session injects into every service: `MICROMUX_SESSION`, `MICROMUX_SESSION_DOMAIN`, `MICROMUX_PROXY_PORT`, `MICROMUX_HOST_SUFFIX`, and every [allocated port]({{< relref "../configuration/sessions.md" >}}#allocated-ports).
- With `--service <id>`, that service's `env_file` and `environment` entries, interpolated, with the session's variables applied. It takes the service id, not its display name. While the service runs, these are the values its process started with; when its config changed since, for example through `ctl reconcile`, a note on stderr says so. The variables micromux sets on every service process itself (`TERM`, `CLICOLOR`, `CLICOLOR_FORCE`, `FORCE_COLOR`) are not included.
- Variables your shell already has, such as `PATH`, `AWS_PROFILE`, or a `DATABASE_URL` from direnv, come from your shell. `exec` keeps them, and only the session's configured variables override variables of the same name. The environment micromux itself was started with, which its services also inherit, is not part of it.

### `micromux exec`

The command replaces the micromux process: it runs in your terminal with your stdin, stdout, and stderr, receives your signals directly, and its exit status is the exit status of `micromux exec`. Everything after the command name belongs to the command, so `micromux exec ls -la` needs no `--`, and `--` works as well.

| Code | Meaning |
|---|---|
| `125` | micromux failed before running the command: no session, a session too old for it, an unknown service, or a control-plane error. |
| `126` | The command was found but cannot run. |
| `127` | The command was not found. |
| any other | The command's own exit status. |

### `micromux env`

- By default it prints sorted `KEY=value` lines. A value is single-quoted for a POSIX shell when it contains anything but letters, digits, and `_@%+=:,./-`.
- `--export`, or `--format shell`, prints `export KEY=value` lines for `eval`.
- `--format json` prints one JSON object.
- Names after the options print only those variables, and a single name in the default format prints its bare value. A name that is not set fails the command, and nothing goes to stdout, so a script never runs with an empty port.
- A key a shell cannot assign, anything but `[A-Za-z_][A-Za-z0-9_]*`, is left out of the line formats with a warning on stderr that names it. JSON keeps every key.
- It exits 1 on any failure.

> [!WARNING]
> Values can be secrets from env files. They reach stdout or the command only when you run one of these commands, and micromux never logs them, unless you replace its log filter with a `RUST_LOG` that turns on debug logging for the MCP library (`rmcp`), which records tool results. The control socket is reachable by your user only.

Agents read the same environment with the MCP `get_session_env` tool. For a service, it returns only the variable names unless the agent asks for `names`.

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

Protocol 3 peers accept additive fields from newer minor revisions. Revision 3.8 changed transient disk-log rotation and reader saturation failures from `LimitExceeded` to the retryable `Busy` code. Revision 3.9 distinguishes an uninitialized disk-reader pool and reports reads that still occupy workers after their callers leave. Revision 3.11 adds `session` to the session description: its name, [browser URLs and allocated ports]({{< relref "../configuration/sessions.md" >}}), and whether the shared proxy serves its routes. Revision 3.12 adds `last_cleanup` to service snapshots and the `CleanupStarted`, `CleanupFinished`, and `CleanupFailed` event kinds for [cleanup commands]({{< relref "../configuration/services.md" >}}#cleanup); older peers read the new kinds as `Unknown`. Revision 3.13 adds `dependency_failure` to service snapshots, naming the dependency that keeps a blocked service from ever starting on its own, and the `DependencyFailed` event kind. Revision 3.14 adds `depends_on` to service snapshots. Revision 3.15 adds the `GetEnvironment` request, which returns the session's variables or one service's resolved environment with their values; `micromux env`, `micromux exec`, and `get_session_env` refuse a session older than 3.15 and ask you to restart it. Revision 3.16 adds [jobs]({{< relref "../configuration/services.md" >}}#jobs): `kind` and `manual` on service snapshots and dynamic-service definitions, the `Completed` and `Failed` execution states, and an optional dependency `condition` whose absence means the target's natural condition. Older peers read the new states as `Unknown`, so restart an attached TUI or a running `micromux mcp` after upgrading; an older session ignores `kind` and creates a dynamic job as a service.
