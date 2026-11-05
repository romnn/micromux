---
title: Sessions and ports
weight: 6
---

# Sessions and ports

Several checkouts of one project, such as git worktrees, can each run their own micromux session at the same time. Two things keep them apart: every session has a **name**, and a session can ask micromux to **allocate ports** instead of hard-coding them.

```yaml
session:
  name: luup2          # optional; defaults to the config directory's name, slugified
  domain: localhost    # optional; defaults to "localhost"

allocate_ports:
  - API_PORT
  - POSTGRES_PORT
```

## Session name

`session.name` is a lowercase hostname label (`a-z`, `0-9`, and inner hyphens, at most 63 characters). Without it, micromux takes the name of the directory the config lives in and slugifies it: `/work/My Project/micromux.yaml` becomes `my-project`. `session.domain` is the parent domain the session's hostnames live under.

Two live sessions cannot share a name. When a second session would, it fails to start and asks you to set `session.name`.

## Allocated ports

Every name under `allocate_ports` becomes an environment variable holding a free TCP port:

- The port **stays the same across restarts** of the session. Assignments are stored per config file in micromux's state directory (`~/.local/state/micromux/ports` on Linux, `~/Library/Application Support/com.romnn.micromux/ports` on macOS).
- **No two live sessions of the same user share a port.** A session holds its assignment while it runs; a stopped session's ports are only handed to another session when no other port is left.
- Ports come from `20000`–`32767`, below the ephemeral port ranges of Linux and macOS, so outgoing connections never take one.
- A port a session is assigned for the first time is checked to be free. If a port the session had before is now held by some other process, the session **fails to start** and names the variable and the port, rather than pointing your services at a stranger. Stop that process (often a leftover of an earlier run) and start again.

Only one micromux can run a given config file at a time; a second one fails to start.

Names must be environment variable names. `MICROMUX_*` names and the variables micromux sets on every service itself (`TERM`, `CLICOLOR`, `CLICOLOR_FORCE`, `FORCE_COLOR`) are reserved.

## Injected variables

These variables are available to every [interpolation]({{< relref "services.md" >}}#variable-interpolation) — `env_file` contents, `environment`, `ports`, `command`, `healthcheck.test`, and the `working_dir` and `env_file` paths — and are set in every service process, dynamic services included:

| Variable | Value |
|---|---|
| `MICROMUX_SESSION` | The session name, such as `luup2`. |
| `MICROMUX_SESSION_DOMAIN` | `<name>.<domain>`, such as `luup2.localhost`. |
| every `allocate_ports` name | Its allocated port. |

**They win over every other definition.** An `env_file` line or `environment` entry of the same name is ignored, so an existing `.env` that says `API_PORT=9540` keeps working outside micromux, while inside micromux the allocated port replaces it, and a later line such as `API_URL=http://127.0.0.1:${API_PORT}` sees the allocated value.

## Restarting to apply changes

`session` and `allocate_ports` are read once, when the session starts. A config reload — [`reconcile_config`]({{< relref "../agent-control/control-plane.md" >}}), or a restart that re-reads the config — that changes them is rejected; restart the session to apply the change.

`validate_config` never reserves anything. It uses the assignment a session already has, or provisional values for names that were never assigned, and says so in a note.
