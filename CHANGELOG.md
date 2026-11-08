# Changelog

## Unreleased

- **Sessions get a name and stable, collision-free allocated ports.** Two checkouts of one
  project could not run side by side, because both started their services on the ports in their
  `.env`. `session: {name, domain}` names a session, defaulting to its config directory's name
  slugified to a hostname label under `localhost`, and `allocate_ports: [VAR, ...]` gives every
  listed variable a free TCP port. A name keeps its port across restarts, since the assignment is
  persisted per config file, and no two live sessions of the same user share one.
  `session.port_range` picks the inclusive range ports come from (default `20000`-`32767`), so a
  tool that hands out host ports the same way, such as a testcontainers harness, does not race
  micromux for them. `MICROMUX_SESSION`, `MICROMUX_SESSION_DOMAIN` and every allocated port reach
  every interpolation and every service process, dynamic services included, and override
  `env_file` and `environment` entries of the same name. A persisted port that a foreign process
  now holds fails startup instead of pointing services at it, as do a second micromux on the same
  config and a second live session with the same name. Session settings are latched, so a reload
  that changes them is rejected.

- **A shared per-user HTTP proxy serves each session's services at their own hostnames.**
  `routes` maps hosts under the session domain, such as `http://app.<session>.localhost`, or a
  `"*"` catch-all, to a port, and one `micromux proxy` shared by all of a user's sessions forwards
  requests by `Host`, WebSocket upgrades and streamed responses included. A route can also take a
  `path` prefix, where the longest matching prefix wins, and `strip_prefix` hides that prefix
  from the upstream, so one host can split between services without a second reverse proxy. The
  proxy listens on `proxy.port` (default `80`) on `127.0.0.1` and `[::1]`, uses
  `proxy.fallback_port` only when that port cannot be bound, answers an unknown host with a page
  listing the live sessions and a refused upstream with a page naming the owning service, and
  exits 60 seconds after its last session leaves. Sessions get `MICROMUX_PROXY_PORT` and
  `MICROMUX_HOST_SUFFIX`, and after a proxy crash a session restarts it on the same port. A config
  reload republishes which service owns each route without ending the session's open
  connections. A session that needs a newer proxy than the one running asks an idle outdated
  proxy to stop, or waits for it to exit, bounded at 75 seconds, and refuses one that still serves
  sessions, naming them; `start_session` waits out that bound instead of reporting a timeout.
  `routes` are rejected on Windows.

- **Session URLs and allocated ports are visible over MCP, `ctl` and the TUI.** The new MCP tool
  `get_session_urls` and `list_sessions` return the session's routes with their browser URLs,
  owning services and allocated ports, `start_session` reports them as `urls`, and
  `micromux ctl describe` prints them. In the TUI, the logs pane's title lists the selected
  service's URLs, which a click opens, `i` opens a popup with everything about the selected
  service (state, URLs, ports by variable, command, directory, dependencies, restart and stop
  policy, last healthcheck, current run and last cleanup), and `u` opens one for the session's
  domain, proxy, routes and ports. The services list no longer shows ports in brackets, and a red
  header row says when the proxy is not serving the session's routes.

- **Jobs run to completion alongside services.** A service with `kind: job` completes on exit
  status 0 and fails on any other, and never restarts unless it sets `restart: on-failure[:N]`. A
  bare `depends_on` entry on a job waits for it to complete, and a failed job fails its dependents
  at once. `run: manual` keeps a job from running until it is restarted, which
  `ensure_service_ready` also does for a manual job that has not run. Completed jobs render gray
  and frozen in the TUI, failed ones red, `ctl ls` prints `kind=job`, and the waits treat
  completion as ready. A job cannot have a healthcheck, and `run: manual` cannot be combined with
  `disabled: true`. Older clients read the new states as unknown, so restart an attached TUI or a
  long-running `micromux mcp` after upgrading.

- **Waits fail fast behind a dependency that cannot recover, and `micromux ctl wait` brings the
  same wait to scripts.** A service blocked behind a dependency, direct or transitive, that exited
  for good, is disabled, retired or missing used to leave `wait_for_healthy` hanging until its
  timeout. The blocked service now records that dependency, its state and exit code, which
  `ctl ls` shows as `failed_dependency=...`, and `wait_for_healthy`, `wait_for_exit` and the
  bundles built on them report it at once. `micromux ctl wait <service> [--healthy | --exit]
  [--timeout <dur>] [--after-generation <n>]` exits `0` when the condition is met, `3` when the
  run exited, `4` on a failed dependency, `5` when the service is disabled or retired and `124` on
  timeout. Control commands are now acknowledged after the scheduling pass they trigger, so a
  wait right after a restart sees its effects.

- **`micromux exec` and `micromux env` run commands with, and print, a live session's
  environment.** A plain shell only saw the `.env` ports, so dev tasks reached the wrong stack.
  `micromux exec [--session <selector>] [--service <id>] [--] <cmd> [args...]` replaces itself
  with the command under the session's variables, or under a service's resolved environment with
  `--service`. `micromux env` prints the same as sorted, POSIX-quoted `KEY=value` lines, as
  `export` lines with `--export`, as JSON with `--format json`, or a single bare value for
  `$(micromux env API_PORT)`. Both resolve the session the way `ctl` does and fail rather than
  fall back to `.env` when none runs. The MCP tool `get_session_env` returns the session variables,
  and a service's values only when they are asked for by name.

- **A service's `cleanup` command runs after each of its processes exits.** A service that runs,
  say, `docker compose up` in the foreground can set `cleanup: {command: docker compose down,
  timeout: 2m}`, and stop, disable, restart, a crash and session shutdown all leave a clean slate
  before the service counts as stopped or its next run starts. The timeout defaults to 60 seconds
  (at most one hour), past which the cleanup gets the stop signal and then SIGKILL. Its output is
  mirrored into the run's log as `[cleanup]` lines and kept in a file in the state directory, and
  `diagnose` flags a failed cleanup, which never blocks the session. A dynamic service cloned with
  `from_service` does not inherit the cleanup, so stopping it cannot tear down what the configured
  service still uses.

- **Service process trees no longer outlive a micromux that dies, SIGKILL included.** On Unix every
  service and healthcheck command now runs under a small shim, the same binary, that watches the
  supervisor and, when it is gone, sends the service's stop signal to everything below it and
  SIGKILLs what remains after the grace period. On Linux the shim is a child subreaper, so
  descendants that started their own session stay reachable; on macOS, processes orphaned before
  the supervisor died can still escape. The cleanup command runs in that case too, and a session
  restarted for the same config waits for the previous session's teardown and cleanups, bounded
  by the deadlines they recorded.

- **`MICROMUX_STATE_DIR` gives micromux a separate per-user setup.** The port registry, cleanup
  state, control sockets, session locks, the shared proxy, logs and run-log spools all move under
  it, so tests and sandboxes no longer touch the developer's sessions; `ctl`, `attach`, `env`,
  `exec` and the MCP server find a session only under the same `MICROMUX_STATE_DIR`. Taking the
  port registry lock now gives up after one holder keeps it for 5 seconds, naming the lock file and
  the holder's pid. A diagnostic probe that happened to hold a starting session's endpoint lock no
  longer disables that session's control plane.

- **A session claims its config before waiting on startup.** A session took its config's owner
  lock only once its control plane came up, after waiting for a previous session's cleanups and
  for an outdated proxy, so a retried or concurrent start for the same config waited alongside it
  and then raced it. The second start now fails at once, naming the holder, and `start_session`
  reports the first as still starting. `start_session` and the `ensure_service_ready` auto-start
  extend their deadline by the cleanup wait the session prints instead of stopping a start that is
  still waiting.

- **The TUI has mouse support, copies only on request, and quits only on `q`.** A click selects a
  service or focuses a pane, the wheel scrolls the pane under the pointer, a drag, double click or
  triple click selects text within one pane, and a click outside a popup closes it. Selecting
  only highlights; `y`, Enter, a right-click, the footer's `y Copy` and the terminal copy shortcuts
  Ctrl+Shift+C, Ctrl+Insert and Super+C copy through OSC 52, with `pbcopy`, `wl-copy`, `xclip` or
  `xsel` as a fallback outside ssh. Capture is on by default and switches with `m`, `--mouse`, or
  `ui.mouse` in the project config or in a new user config at
  `$XDG_CONFIG_HOME/micromux/config.yaml`, which also holds `ui.copy_on_select`. Ctrl-C no longer
  quits the TUI, so a copy shortcut that arrives as Ctrl-C cannot stop a session; it closes popups
  and points at `q`. Ctrl-Z suspends micromux with its whole job until `fg`. The focused pane has
  a thick, accented frame. `MICROMUX_DEBUG_KEYS=1` logs every key micromux reads.

- **PTY input mode is left with `Ctrl+]` and passes the terminal's own keys through.** `Alt+Esc`
  left the mode only where the terminal reported it apart from Esc, so a terminal without the
  kitty keyboard protocol had no way out with mouse capture off. `Ctrl+]` leaves it in every
  terminal, and `Alt+Esc` still does where it arrives apart. While the mode lasts, micromux turns
  the kitty protocol and tmux's modifyOtherKeys off, so services receive the bytes a terminal sends
  without micromux, which also fixes Ctrl chords on non-Latin layouts, Ctrl+Space, modified
  arrows, Home/End/PageUp/PageDown, F1 to F12 and Ctrl+Backspace.

- **Session discovery probes only session sockets.** When the runtime directory fell back to
  `/tmp/micromux-<uid>`, discovery probed the shared proxy's socket as a session. It now considers
  only `<endpoint hash>.sock` files, and a `hash:` selector that is not an endpoint hash names no
  session instead of being turned into a socket path.

- **Command displays are shell-quoted.** A command copied from the TUI, `ctl health`, MCP or a
  cleanup log now runs the same argv when pasted into a POSIX shell; a `CMD-SHELL` healthcheck
  reads `sh -c "echo 'ok'"` instead of `sh -c echo 'ok'`.

- **A change to only a service's environment marks it stale.** Editing an `environment` entry or a
  value in an `env_file` did not set `config_stale`, so the TUI and `ctl` called the service
  current. The comparison now covers the whole run definition, including the resolved environment
  and the full healthcheck, and `ctl ls` marks such a service `config=stale`.

- **A short-lived service no longer gets a blank last line it never wrote.** The PTY writer typed a
  newline when it was dropped at run teardown, which the terminal echoed behind the service's
  output, readily under load.

## v0.3.0

- **The TUI filters structured logs per service by level, timestamps and fields.** The `logs`
  block gains `level`, `timestamps`, `filter_fields` and a `fields` map of `hide`/`show`; top-level
  values are defaults, a service replaces the scalars and merges `fields` key by key, and a config
  reload applies them without a restart. In the TUI, `L` picks a level threshold, `T` toggles
  timestamps and `F` toggles the hidden fields for the selected service, and lines without a
  structured level always show. Agents still read every record and field. The header now carries
  the version and view toggles, the footer the actions, both wrap on narrow terminals, and `?`
  opens a help overlay.

- **Services run in the config directory unless they set `working_dir`.** A service without
  `working_dir` spawned in the home directory, although the docs promised the config's directory.
  Every configured and dynamic service now runs there, and a program with a directory part, such
  as `./bin/api.sh`, resolves against the working directory as a shell would; bare names still use
  `PATH`.

- **A disabled or retired service's logs render in gray.** Its log pane looked like a live one, so
  only the sidebar said the output was a record. Colors now map to grays of their own brightness
  while bold, underline and the other modifiers stay, and re-enabling brings the colors back.

- **`--version` shows the control protocol version,** as in `0.2.0 (protocol 3.10)`, since whether
  a client can talk to a running session from another release depends on the protocol major.

- **Reloading an unchanged config no longer reports every service as changed** when the config
  directory was reached through a symlink, as macOS's temp directory is.

## v0.2.0

- **Variables interpolate consistently across every service field.** **Breaking:** an unset
  variable is now an error instead of an empty substitution, and a shell script in `command` must
  escape its own variables as `$$var`. Every value that reaches a service process resolves against
  one layered environment: the supervisor's environment, then each `env_file` in order, then
  `environment`; `working_dir` and `env_file` paths see only the supervisor's environment.
  `command` and `healthcheck.test` interpolate per argument after splitting, so a quoted `"${VAR}"`
  stays one argument. Supported forms are `$VAR`, `${VAR}`, `${VAR:-default}` and
  `${VAR-default}`, and `$$VAR` keeps a literal, while shell syntax such as `$1`, `$?` and `$(...)`
  passes through. An `optional: true` env file is also skipped when its path names an unset
  variable. Validation and startup report every unresolvable reference with its service, field and
  variable, for all failing services at once.

## v0.1.4

- **Stopping a service reaps descendants that left its process group.** Processes that moved into
  their own group with `setsid` or `setpgid`, such as turborepo task runners and daemonizing dev
  servers, survived termination and leaked. Termination now snapshots the service's descendant
  tree and signals escapees individually, by pid and start time so a recycled pid is never hit.

- **A service's `stop_signal` sets the signal it is asked to stop with.** It defaults to
  `SIGTERM` and accepts `INT`, `HUP`, `QUIT`, `USR1` and `USR2`, case-insensitive with an optional
  `SIG` prefix.

- `restart: on-failure` counts now need a separator, as the schema says: `on-failure3` is
  rejected, while `"on-failure : 3"` is accepted.

## v0.1.3

- Fix healthchecks that failed with "no such file or directory" after a config reconcile replaced
  their service.

## v0.1.2

- **`stop_grace_period` sets how long a service gets to stop before it is force-killed.** It
  defaults to `10s` and must be greater than zero and at most `5m`; session shutdown waits for the
  longest grace period among the running services.

- **Disk-backed run logs are bounded.** Each retained run log rotates in place at 64 MiB, keeping
  its newest segment, so one noisy long-lived process cannot fill the disk.

- **MCP `start_session` needs `micromux mcp --allow-session-start`.** Without that flag (or
  `MICROMUX_MCP_ALLOW_SESSION_START`) the tool refuses to spawn processes, and permitted starts are
  capped at eight per minute. The docs now state that the same-user control interface reports
  paths, argv and probe output to its clients and is not a redacted security boundary.

- **Supervision and logging hold up under stalled children and slow clients.** Termination, PTY
  input, healthchecks, disk log reads and control responses are bounded, so a wedged child or
  reader can no longer stall the scheduler or grow memory without limit. Output a service writes
  just before it exits is drained before it is reaped, a paste in PTY input mode is delivered
  atomically, oversized control responses are paginated, an attached TUI keeps its reconnect
  backoff, and on Windows each service runs in a kill-on-close job object so its tree ends with
  micromux.

## v0.1.1

- **`env_file` entries can be optional.** A missing `optional: true` env file is skipped instead of
  failing the service; a present one still reports parse errors.

- **A service held back by unmet dependencies shows as blocked.** It idled as pending, or as
  exited after a restart, so a gated dependent looked crashed. Lifecycle transitions that stall a
  log, such as disable, restart, dependency waits, spawn failure, backoff and retire, are also
  written into the log as banner lines, so a frozen log ends with the reason.

## v0.1.0

- **Coding agents can discover and control running sessions through an MCP server.**
  `micromux mcp` serves tools over stdio to list sessions and services, read and follow logs,
  restart, enable and disable services, inspect health and wait for it, among them
  `list_sessions`, `find_service`, `get_logs`, `follow_logs`, `follow_all_logs`, `wait_for_log`,
  `get_health`, `get_health_history`, `wait_for_healthy`, `wait_for_exit`,
  `restart_service_and_wait`, `ensure_service_ready`, `diagnose` and `validate_config`.
  `start_session` and `stop_session` start a headless session for a project or stop one and free
  its ports. Log tools strip ANSI by default and filter by `grep`, `grep_context`, time, trace id
  and, for JSON logs, `min_level`, with a token-efficient `format: "compact"`. Actions go through
  the same control plane as the TUI, so dependency gating, healthchecks and restart policy apply.

- **Each session serves a local control endpoint, driven by `micromux ctl` and `micromux serve`.**
  A Unix socket per config, keyed by its canonical path, carries a versioned protocol that
  tolerates additive changes between builds, so a newer client keeps talking to an older session
  of the same major version. `micromux ctl` lists, restarts, enables, disables, describes and
  stops sessions and services and reads logs and health, `micromux serve` runs the supervisor
  headless, and `--no-control` or `control: {enabled: false}` turns the endpoint off. An optional
  top-level `name` names the session. Discovery searches the project, `XDG_RUNTIME_DIR` and
  temporary runtime directories, and reports busy or version-mismatched endpoints. The control
  plane, and with it `attach`, `ctl`, `mcp` and `serve`, is not yet available on Windows.

- **`micromux attach` opens the TUI on a running session.** It mirrors a headless session, such
  as one an agent started, without a second supervisor, and several clients can attach at once.
  The logs pane's border shows the selected service's command, run generation and, for dynamic
  services, revision and lease.

- **Agents can start runtime services next to the configured ones.** With
  `control.dynamic_services.enabled: true`, `start_dynamic_service`,
  `start_dynamic_service_and_wait`, `replace_dynamic_service`, `renew_dynamic_service` and
  `stop_dynamic_service` manage leased services bounded by `allowed_working_roots`, `max_services`
  and `max_lifetime`, with idempotency keys for safe retries; `get_service_events` explains their
  lifecycle. The policy is off by default and latched when the session starts.

- **Config edits apply without stopping the session.** Restart, enable and automatic restarts
  reload the service's definition from disk, and `reconcile_config` or `micromux ctl reconcile`
  adds, retires and updates configured services in a running session, with a dry run that shows
  the diff. Snapshots flag a running service whose config has changed.

- **Recent runs' logs are retained on disk.** Each service keeps `logs.retained_runs` runs
  (default `5`), so the output of a run that crashed stays readable after a restart through
  `list_log_runs`, `get_logs` and `micromux ctl log-runs`. Top-level `restart`, `healthcheck` timing
  and `logs` settings are now defaults every service inherits and can override.

- **The TUI pretty-prints structured JSON logs.** JSON log lines render as compact colored
  records, kept intact after non-JSON build output; `--no-pretty-json-logs` or
  `ui.pretty_json_logs: false` turns it off.

- **Services can start disabled with `disabled: true`,** and be enabled later from the TUI or the
  control plane.

- **Healthchecks behave as documented.** An omitted `interval` or `timeout` now defaults to 30
  seconds, where it busy-spawned probes or let a hung probe block forever. A service keeps being
  probed after it turns unhealthy, so it can recover; it turns unhealthy after exactly `retries`
  failures; a probe cancelled by a stop or restart no longer counts as healthy; a probe whose
  background child holds its output open no longer blocks the loop; and health reflects only the
  current run.

- **Restart and shutdown are reliable.** A service in restart backoff now restarts when the backoff
  expires instead of waiting for an unrelated event, backoff applies to every automatic restart,
  and `restart: on-failure` without a count retries without limit. Force-killing a service kills
  its whole process group, and shutdown always drains children, even when the TUI ends abnormally.
  `R` no longer re-enables disabled services.

- **Config mistakes are reported.** Unknown service and `ui` keys and a missing `services` section
  warn, or fail with `--strict`, and a reference to an unset variable warns instead of silently
  expanding to nothing. Single-quoted `env_file` values stay literal, so a `$` in a password
  survives, and an unbalanced quote no longer disables comment stripping.

- **TUI and logging fixes.** Line-by-line scrolling reaches the last wrapped row, the header shows,
  interactive screen snapshots no longer overwrite unrelated log lines after a dropped frame or a
  restart, blank lines are preserved, a stray carriage return no longer adds a blank record, and
  exited and killed services are styled apart from running ones. `-v` and `-q` set the log level,
  file logging works, old log files are pruned after a week, and the terminal is restored when
  micromux fails.

- Release archives are built for arm64 and amd64 on Linux, macOS and Windows, and the
  documentation moves to a site at romnn.github.io/micromux.

## v0.0.7

- The Homebrew cask declares macOS and Linux support.

## v0.0.6

- Clear a service's logs when it restarts.

## v0.0.5

- Add a `--version` flag.

## v0.0.4

- Emulate a full terminal for service output with `alacritty_terminal`, so interactive programs
  such as `htop` and `watch` render correctly.

## v0.0.3

- Update dependencies; no user-visible changes.

## v0.0.2

- First release: a local process supervisor with a terminal UI that runs the services in
  `micromux.yaml` with Compose-style `depends_on` conditions, healthchecks and restart policies.
- Show each service's logs with ANSI color, restart a service with `r` or all with `R`, disable and
  enable one with `d`, and send input to a service in PTY-backed attach mode with `a`.
