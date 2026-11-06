---
title: Sessions, ports, and hostnames
weight: 6
---

# Sessions, ports, and hostnames

Several checkouts of one project, such as git worktrees, can each run their own micromux session at the same time. Three things keep them apart: every session has a **name**, a session can ask micromux to **allocate ports** instead of hard-coding them, and a shared **proxy** serves each session's HTTP services under its own hostnames, such as `http://app.luup2.localhost`.

```yaml
session:
  name: luup2          # optional; defaults to the config directory's name, slugified
  domain: localhost    # optional; defaults to "localhost"
  port_range: { start: 30000, end: 32767 }   # optional; defaults to 20000-32767

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
- Ports come from `session.port_range`, by default `20000`–`32767`, below the ephemeral port ranges of Linux (`32768`+) and macOS (`49152`+), so outgoing connections never take one. See [choosing a port range](#choosing-a-port-range).
- A port a session is assigned for the first time is checked to be free. If a port the session had before is now held by some other process, the session **fails to start** and names the variable and the port, rather than pointing your services at a stranger. Stop that process (often a leftover of an earlier run) and start again.

Only one micromux can run a given config file at a time; a second one fails to start.

### Choosing a port range

A port is checked to be free when micromux assigns it, but nothing reserves it at the operating-system level until your service binds it. Other tools that hand out ports the same way can take it in between. Test harnesses are the usual culprit: testcontainers setups that give containers fixed host ports often pick them below the ephemeral range too, for the same reason micromux does, and a busy test run can then grab a port moments before your `docker compose up` binds it ("address already in use").

If such a tool runs on the machine, give micromux a range of its own that does not overlap:

```yaml
session:
  port_range: { start: 30000, end: 32767 }
```

`start` and `end` are inclusive, at least `1024`, and the range must have room for every `allocate_ports` name (the proxy's ports inside it do not count). Staying below `32768` keeps it clear of Linux's ephemeral ports. When the range changes, a session's next start keeps the ports already inside the new range and assigns new ones for the rest.

Names must be environment variable names. `MICROMUX_*` names and the variables micromux sets on every service itself (`TERM`, `CLICOLOR`, `CLICOLOR_FORCE`, `FORCE_COLOR`) are reserved.

## Injected variables

These variables are available to every [interpolation]({{< relref "services.md" >}}#variable-interpolation) — `env_file` contents, `environment`, `ports`, `command`, `healthcheck.test`, and the `working_dir` and `env_file` paths — and are set in every service process, dynamic services included:

| Variable | Value |
|---|---|
| `MICROMUX_SESSION` | The session name, such as `luup2`. |
| `MICROMUX_SESSION_DOMAIN` | `<name>.<domain>`, such as `luup2.localhost`. |
| `MICROMUX_PROXY_PORT` | The port the shared proxy listens on, such as `80`. |
| `MICROMUX_HOST_SUFFIX` | What follows `<route>.` in a browser URL: `luup2.localhost` when the proxy listens on port 80, `luup2.localhost:8080` otherwise. For example `VITE_APP_URL=http://app.${MICROMUX_HOST_SUFFIX}`. |
| every `allocate_ports` name | Its allocated port. |

**They win over every other definition.** An `env_file` line or `environment` entry of the same name is ignored, so an existing `.env` that says `API_PORT=9540` keeps working outside micromux, while inside micromux the allocated port replaces it, and a later line such as `API_URL=http://127.0.0.1:${API_PORT}` sees the allocated value.

## Hostnames

`routes` gives services hostnames under the session's domain, served by one proxy shared by all of your sessions:

```yaml
routes:
  - host: app                 # http://app.<session>.<domain>
    port: ${APP_PORT}
  - host: email.internal      # several labels are fine
    port: ${MAILPIT_PORT}
  - host: "*"                 # catch-all: every other name under the session domain
    port: ${GATEWAY_PORT}
```

Browsers resolve every `*.localhost` name to the local machine on their own, so nothing has to be added to `/etc/hosts`. Other programs may not (macOS's resolver does not), so services keep talking to each other over `127.0.0.1:<port>`; the hostnames are for browsers and browser tests.

- `port` is the upstream port on `127.0.0.1`. It may reference micromux's own environment and the [injected variables](#injected-variables), typically an `allocate_ports` name, but not a service's `env_file`. A route cannot point at the proxy's own port.
- The proxy routes by the `Host` header, which reaches the service unchanged, and passes WebSocket upgrades and streaming responses (SSE, chunked) through without buffering.
- A hostname no session serves gets a 404 page listing the live sessions and their URLs. A route whose service does not accept connections gets a 502 page naming the session and the service. The service is the one whose `ports` list the route's port, so list the port there.
- `routes` are not supported on Windows.

### The proxy

The first session with routes starts the proxy (`micromux proxy`) in the background. Sessions keep their routes registered over a local socket for as long as they run; a session that crashes loses its routes within seconds, and the proxy exits on its own a minute after the last session is gone.

The proxy listens on `127.0.0.1` and `[::1]` (Linux resolves `*.localhost` to `::1` first). It uses **port 80** unless configured otherwise, and it never quietly moves to another port:

```yaml
proxy:
  port: 80             # default
  fallback_port: 8080  # used only when port 80 cannot be bound
```

On Linux, port 80 needs privileges unless you allow unprivileged low ports once with `sudo sysctl -w net.ipv4.ip_unprivileged_port_start=80` (persist it in `/etc/sysctl.d/`) or grant the binary `cap_net_bind_service` with `setcap`. Without either, set `fallback_port`, or the session fails to start and says so. macOS lets unprivileged processes bind port 80 only on the wildcard address, so there the proxy listens on all interfaces and drops every connection that does not come from the local machine.

A session joins a proxy that is already running only if it listens on `port` or `fallback_port`. The port a session started with stays its port: if the proxy goes away, the session starts it again on the same port, because its services already hold URLs with that port.

## Restarting to apply changes

`session` (including `port_range`), `allocate_ports`, `routes`, and `proxy` are read once, when the session starts. A config reload — [`reconcile_config`]({{< relref "../agent-control/control-plane.md" >}}), or a restart that re-reads the config — that changes any of them is rejected; restart the session to apply the change.

`validate_config` never reserves anything. It uses the assignment a session already has, or provisional values for names that were never assigned, and says so in a note.
