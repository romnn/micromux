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

- The port **stays the same across restarts** of the session. Assignments are stored per config file in micromux's state directory (`~/.local/state/micromux/ports` on Linux, `~/Library/Application Support/com.romnn.micromux/ports` on macOS, or [`MICROMUX_STATE_DIR`](#isolated-state)).
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

`start` and `end` are inclusive, at least `1024`, and the range must have room for every `allocate_ports` name (the proxy's ports inside it do not count). Staying below `32768` keeps it clear of Linux's ephemeral ports. `30000`–`32767` is also the default Kubernetes NodePort range: if a Kubernetes distribution runs directly on the host (k3s, microk8s, rke2), it can intercept connections to those ports even though nothing listens on them, so pick another range there. When the range changes, a session's next start keeps the ports already inside the new range and assigns new ones for the rest.

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

To use them from your own shell, for a migration or a seed script against the session's services, run the command with `micromux exec` or print them with `micromux env`; see [Run commands in the session environment]({{< relref "../agent-control/control-plane.md" >}}#run-commands-in-the-session-environment).

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
- A hostname or path no route serves gets a 404 page listing the live sessions and their URLs. A route whose service does not accept connections gets a 502 page naming the session and the service. The service is the one whose `ports` list the route's port, so list the port there. A config reload that adds, removes, or changes services updates which service that is.
- `routes` are not supported on Windows.

### Path routes

A route with a `path` serves only the requests under that path prefix, so one hostname can split between services, and `strip_prefix` hides a prefix the upstream does not serve:

```yaml
routes:
  - host: api
    path: /rest/              # http://api.<session>.<domain>/rest/...
    port: ${API_HTTP_PORT}
  - host: api                 # every other path of api
    port: ${API_GRPC_WEB_PORT}
  - host: auth
    path: /auth/v1/
    strip_prefix: true        # the upstream sees /token, not /auth/v1/token
    port: ${AUTH_PORT}
```

- `path` starts with `/` and is a plain prefix of the request path, without the query: `/rest/` matches `/rest/users` and `/rest/`, but not `/rest` or `/restore`. Matching is case-sensitive and sees the path as the browser sent it, without decoding percent-escapes or resolving `..`. To match both `/rest` and `/rest/...`, write `path: /rest`, which also matches `/restore`.
- The host decides first, then the path. A hostname that any route names is served by its own routes alone; the catch-all `*` routes serve only the hostnames no route names. Among those routes, the longest matching `path` wins, and a route without `path` serves every path no other route matches. A path none of them matches gets a 404, even when a catch-all exists, so a mistyped API path never lands on a frontend.
- `strip_prefix: true` removes the matched `path` before forwarding and keeps the query: `/auth/v1/token?x=1` reaches the upstream as `/token?x=1`. What remains always starts with `/`, so `/auth/v1/` arrives as `/`. The `Host` header still reaches the service unchanged. It needs a `path`.
- A host may appear once without a path and once per distinct path. Each route has its own URL, such as `http://api.<session>.<domain>/rest/`, and its own service on error pages, in `ctl describe`, `get_session_urls`, and the TUI.
- Path routes carry every kind of traffic the proxy forwards: plain requests, WebSocket upgrades, server-sent events, and streamed bodies. The proxy does not rewrite headers, redirects, or response bodies; a service behind a stripped prefix that builds absolute links has to know its public prefix itself.

### The proxy

The first session with routes starts the proxy (`micromux proxy`) in the background. Sessions keep their routes registered over a local socket for as long as they run; a session that crashes loses its routes within seconds, and the proxy exits on its own a minute after the last session is gone.

The proxy listens on `127.0.0.1` and `[::1]` (Linux resolves `*.localhost` to `::1` first). It uses **port 80** unless configured otherwise, and it never quietly moves to another port:

```yaml
proxy:
  port: 80             # default
  fallback_port: 8080  # used only when port 80 cannot be bound
```

On Linux, port 80 needs privileges unless you allow unprivileged low ports once with `sudo sysctl -w net.ipv4.ip_unprivileged_port_start=80` (persist it in `/etc/sysctl.d/`) or grant the binary `cap_net_bind_service` with `setcap`. Without either, set `fallback_port`, or the session fails to start and says so. macOS lets unprivileged processes bind port 80 only on the wildcard address, so there the proxy listens on all interfaces and drops every connection that does not come from the local machine.

A session joins a proxy that is already running only if it listens on `port` or `fallback_port`. A session with path routes also needs a proxy that routes by path: one started by an older micromux would ignore the paths, so the session never registers with it.

After an upgrade, the old proxy usually serves no session anymore and only waits out its idle minute, so a session that needs a newer proxy takes it over. A proxy from this version on is asked to stop right away. An older one cannot be stopped, so the session shows `waiting for the outdated proxy (pid N) to exit` and waits up to 75 seconds for it to exit on its own. Then it starts the new proxy and registers. The session never signals the old proxy's process. If other sessions still use the old proxy, the session refuses it instead and names the proxy's process id and those sessions: stopping it would cut their routes, and an older micromux would start it again. Stop those sessions or restart them with the new micromux; the old proxy exits a minute after its last session stops. Once only sessions of the new micromux use it, `kill <pid>` moves them to a new proxy right away.

The port a session started with stays its port: if the proxy goes away, the session starts it again on the same port, because its services already hold URLs with that port.

## Restarting to apply changes

`session` (including `port_range`), `allocate_ports`, `routes`, and `proxy` are read once, when the session starts. A config reload — [`reconcile_config`]({{< relref "../agent-control/control-plane.md" >}}), or a restart that re-reads the config — that changes any of them is rejected; restart the session to apply the change.

`validate_config` never reserves anything. It uses the assignment a session already has, or provisional values for names that were never assigned, and says so in a note.

## Isolated state

Everything that lets your sessions find each other is per user: the port registry and cleanup state in micromux's state directory, the control sockets in the runtime directory, and the session locks and the shared proxy's socket in `/tmp/micromux-<uid>`. The state directory derives from your account's home directory, not `$HOME` or `$XDG_STATE_HOME`, so sessions started from differently configured shells, agents, and services still share it.

`MICROMUX_STATE_DIR` gives micromux a separate setup in a directory of its own. It is meant for tests and isolated setups, such as a sandbox that runs its own projects:

```sh
MICROMUX_STATE_DIR=/tmp/micromux-test micromux serve
```

- The directory holds everything micromux otherwise keeps per user: the port registry in `ports/`, cleanup files in `cleanup/`, control sockets in `run/`, session locks and the proxy's socket in `coordination/`, and micromux's logs and run-log spools in `cache/`.
- Sessions inside and outside it do not see each other. `micromux ctl`, `attach`, `env`, `exec`, and the MCP server find a session only when they run with the same `MICROMUX_STATE_DIR`.
- They do not exclude each other either: a session with the variable and one without it can run the **same config file at the same time**, and both start its services without a warning. Do not point an isolated setup at a project you also run normally.
- Ports are shared by the whole host. A session in a separate setup skips ports that are in use, but it does not know the ports reserved in your own registry, so it can take one that a stopped session of yours expects back. That session then fails to start with a busy port. Give an isolated setup its own `session.port_range` when it runs services that bind their ports, and its own proxy `port` when it has routes.
- The path must be absolute. micromux creates `run/` and `coordination/` private to you, and the control and proxy sockets live in them, so keep the path short: macOS limits socket paths to 104 bytes.

Registering a session briefly holds a lock on the port registry. A session waits as long as other sessions keep taking turns, but when a single process holds the lock for 5 seconds, the session fails to start and names the lock file and that process, rather than waiting on a stuck process.
