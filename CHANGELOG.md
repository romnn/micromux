# Changelog

## Unreleased

- **Sessions that want different proxy ports share one proxy.** A session no longer fails when
  another project's proxy runs on another port; the proxy listens on each session's `proxy.port`
  and serves its routes only there. `examples/proxy-ports` runs two such projects side by side.

## v0.4.1

- **`E` in the TUI shows the selected service's lifecycle events, the timeline agents read with
  `get_service_events`.** It takes the healthcheck pane's place and works in attached TUIs too.
- **`A` in the TUI shows every service's logs in one pane, interleaved in time order.** Each line is
  tagged with its service, and search, scrolling and copying work as in a service's own pane.
- **`micromux attach` types into services and resizes their PTYs.** Press `a` in an attached TUI;
  it resizes the PTYs only while in PTY input mode, so watching never changes them.
- **`/` searches the selected service's logs in the TUI, and `n` / `N` move between the
  highlighted matches.** Matching is literal and smartcase, and works in attached TUIs too.
- **The TUI draws with your terminal theme's colors instead of fixed ones,** so it stays
  readable in light and dark themes alike. A `▶` in the service's status color marks the selected
  service.
- **Session discovery removes the sockets of crashed sessions, and diagnostics stay small.**
  `list_sessions` and other `socket_probes` reports count dead endpoints in `absent_probe_count`
  and list only a few.
- **An allocated port can prefer a well-known port and takes it whenever it is free.** Write
  `{ name: WEB_PORT, prefer: 3000 }` under `allocate_ports`.

## v0.4.0

- **Sessions get a name and stable, collision-free allocated ports.** Configure them with
  `session: {name, domain, port_range}` and `allocate_ports: [VAR, ...]`.
- **A shared per-user `micromux proxy` serves each session's services at their own hostnames.**
  Map hosts such as `app.<session>.localhost`, optionally by `path` prefix, under `routes`.
- **Session URLs and allocated ports show in MCP, `micromux ctl describe` and the TUI.** New MCP
  tool `get_session_urls`; in the TUI, `i` shows the selected service's details and `u` the
  session's.
- **Services with `kind: job` run to completion.** `depends_on` waits for a job to complete and
  `run: manual` runs one only on request; restart attached TUIs and `micromux mcp` after upgrading.
- **`wait_for_healthy` and `wait_for_exit` fail fast behind a dependency that cannot recover.**
- **`micromux ctl wait <service> [--healthy | --exit]` waits from scripts,** with a distinct exit
  code per outcome.
- **`micromux exec` and `micromux env` run commands with, and print, a live session's
  environment.** The MCP tool `get_session_env` returns the same variables.
- **A service's `cleanup` command runs after each of its processes exits,** such as
  `cleanup: {command: docker compose down}`.
- **Service process trees no longer outlive a micromux that dies, even by SIGKILL,** on Unix.
- **`MICROMUX_STATE_DIR` keeps ports, sockets, the proxy and logs apart from your other
  sessions,** for tests and sandboxes.
- **A second start of the same config fails at once, naming the session that holds it.**
  `start_session` now waits out a previous session's cleanups instead of timing out.
- **The TUI supports the mouse, and selected text is copied only on request.** Toggle capture with
  `m`, `--mouse` or `ui.mouse`; copy with `y`, Enter or a right-click.
- **A user config at `$XDG_CONFIG_HOME/micromux/config.yaml` sets `ui.mouse` and
  `ui.copy_on_select`.**
- **Ctrl-C no longer quits the TUI; press `q`.** Ctrl-Z suspends micromux until `fg`.
- **`Ctrl+]` leaves PTY input mode in every terminal, and the mode passes the terminal's own keys
  through.** This fixes Ctrl chords on non-Latin layouts, modified arrows and function keys.
- **Session discovery no longer mistakes the shared proxy's socket for a session.**
- **Displayed commands are shell-quoted,** so a copied command runs the same when pasted.
- **Changing only a service's environment now marks it stale.**
- **A short-lived service no longer gets a blank last line it never wrote.**

## v0.3.0

- **The TUI filters structured logs per service by level, timestamps and fields.** Configure the
  `logs` keys `level`, `timestamps`, `filter_fields` and `fields`, or toggle with `L`, `T` and `F`.
- **The TUI header and footer wrap on narrow terminals, and `?` opens a help overlay.**
- **Services run in the config directory unless they set `working_dir`,** as documented.
- **A disabled or retired service's logs render in gray.**
- **`--version` shows the control protocol version,** as in `0.2.0 (protocol 3.10)`.
- **Reloading an unchanged config no longer reports every service as changed** when the config
  directory is reached through a symlink.

## v0.2.0

- **Variables interpolate consistently across every service field.** **Breaking:** an unset
  variable is now an error, and a shell script in `command` must escape its variables as `$$var`.

## v0.1.4

- **Stopping a service also stops descendants that left its process group,** such as daemonizing
  dev servers.
- **`stop_signal` sets the signal a service is asked to stop with.**
- **Breaking: `restart: on-failure` counts need a separator,** as in `on-failure:3`.

## v0.1.3

- Fix healthchecks that failed with "no such file or directory" after a config reconcile replaced
  their service.

## v0.1.2

- **`stop_grace_period` sets how long a service gets to stop before it is force-killed.**
- **Disk-backed run logs rotate in place,** so one noisy process cannot fill the disk.
- **Breaking: MCP `start_session` requires `micromux mcp --allow-session-start`.**
- **A wedged child or slow client no longer stalls the supervisor or grows its memory.** Output
  written just before exit is kept, and on Windows a service's process tree ends with micromux.

## v0.1.1

- **`env_file` entries can be `optional: true`.**
- **A service held back by unmet dependencies shows as blocked,** and a stalled log ends with a
  line giving the reason.

## v0.1.0

- **`micromux mcp` serves an MCP server through which coding agents control running sessions.**
  Its tools list services, read and follow logs, restart services and wait for health.
- **Each session serves a local control endpoint, driven by `micromux ctl`.** `micromux serve`
  runs a session headless; not yet available on Windows.
- **`micromux attach` opens the TUI on a running session,** such as one an agent started.
- **Agents can start leased runtime services next to the configured ones.** Enable them with
  `control.dynamic_services.enabled: true`.
- **Config edits apply without stopping the session.** `micromux ctl reconcile` or the MCP tool
  `reconcile_config` adds, retires and updates services.
- **Recent runs' logs are kept on disk,** so a crashed run's output survives a restart; see
  `logs.retained_runs`.
- **Top-level `restart`, `healthcheck` and `logs` settings are defaults every service inherits.**
- **The TUI pretty-prints structured JSON logs.** Turn it off with `--no-pretty-json-logs` or
  `ui.pretty_json_logs: false`.
- **Services can start disabled with `disabled: true`.**
- **Healthchecks behave as documented,** including default timings, retry counts and recovery
  after turning unhealthy.
- **Restart backoff, force-kill and shutdown work reliably,** and `R` no longer re-enables disabled
  services.
- **Config mistakes are reported.** Unknown keys warn, or fail with `--strict`.
- **Single-quoted `env_file` values stay literal,** so a `$` in a password survives.
- **TUI and logging fixes** for scrolling, screen snapshots, blank lines and terminal restore; `-v`
  and `-q` set the log level.
- **Release archives for arm64 and amd64 on Linux, macOS and Windows,** and documentation at
  romnn.github.io/micromux.

## v0.0.7

- The Homebrew cask declares macOS and Linux support.

## v0.0.6

- Clear a service's logs when it restarts.

## v0.0.5

- Add a `--version` flag.

## v0.0.4

- Emulate a full terminal for service output, so interactive programs such as `htop` render
  correctly.

## v0.0.3

- No user-facing changes.

## v0.0.2

- First release: a local process supervisor with a terminal UI that runs the services in
  `micromux.yaml` with Compose-style `depends_on` conditions, healthchecks and restart policies.
- Show each service's logs with ANSI color, restart a service with `r` or all with `R`, disable and
  enable one with `d`, and send input to a service in PTY-backed attach mode with `a`.
