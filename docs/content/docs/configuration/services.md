---
title: Services
weight: 1
---

# Services

Every entry under `services:` is one supervised process. Its key (`api`, `worker`, …) is the service **id** used everywhere else — in `depends_on`, in the TUI, and in `micromux ctl`. Only `command` is required.

```yaml
services:
  api:
    name: API server                # optional display name for the TUI
    command: ["sh", "-c", "./run-api"]
    working_dir: ./services/api
    environment:
      APP_ENV: development
      PORT: "8080"
    env_file: .env
    ports: [8080]
```

## Command

`command` is either a shell-like string or an argv-style array:

```yaml
command: "watch docker ps"                 # split into arguments
command: ["sh", "-c", "npm run dev"]       # exec form, no shell parsing
```

Use the array form when arguments contain spaces or quoting you don't want re-parsed, or when you want a specific interpreter (`["sh", "-c", "…"]`). micromux runs the process in its own process group so restarts and shutdown tear down child processes too.

Services do not outlive micromux, even when it is killed with `SIGKILL` and gets no chance to stop them. On Unix, every service and healthcheck command runs under a small `micromux __shim` process, which notices when micromux is gone, sends the service's `stop_signal` to everything the command started, and force-kills what is left after `stop_grace_period`. On Linux this reaches processes that detached into their own session too. On macOS a process that was already orphaned before micromux died (a double-forked daemon whose parent exited) escapes; everything still attached to the service's tree is stopped. Healthcheck probes get their service's `stop_signal` and `stop_grace_period` too. Two narrow windows remain: a process that detaches in the milliseconds between a forced stop's last process scan and its final kill, if micromux dies exactly then, and the detached descendants of a probe once the probe ends. The pid micromux shows for a service is the shim's: `SIGTERM`, `SIGINT`, `SIGHUP`, `SIGQUIT`, `SIGUSR1` and `SIGUSR2` sent to it are passed on to the command, while other signals, such as `SIGKILL` or `SIGSTOP`, affect only the shim. Signal the command's own pid, or the process group, for anything else. On macOS the shims are named `micromux` too, so `killall micromux` or `pkill micromux` also signals every service directly, on top of the supervisor's own stop; stop a session with `micromux ctl` or by signaling the supervisor's pid instead.

As in a shell, a program with a directory part, such as `./bin/api.sh` or `bin/api.sh`, runs from the service's working directory, while a bare name such as `npm` is looked up on `PATH`.

## Working directory

By default a service and its healthcheck run in the directory the config was loaded from. Override it with `working_dir` (aliases: `cwd`, `directory`), resolved relative to the config file:

```yaml
services:
  web:
    command: "npm run dev"
    working_dir: ./frontend
```

## Environment

Set variables inline with `environment`, load them from files with `env_file`, or both — inline values take precedence:

```yaml
services:
  api:
    command: "./run-api"
    env_file:
      - .env
      - path: ../shared/.env    # the long form is an object with `path`
    environment:
      APP_DEBUG: "false"
```

`env_file` accepts a single path, an object with `path`, or a list mixing the two. Paths resolve relative to the config file. Later files override earlier ones, and `environment` overrides them all.

Mark an entry `optional: true` when the file is not present on every machine. An optional file is skipped when its path names a variable that is unset, or when nothing exists at the path; a file that *is* present is loaded in full, so a syntax error in it still fails the service. That makes a machine-level file shared across several checkouts expressible in a committed config:

```yaml
env_file:
  - .env
  - path: "${SHARED_ENV_FILE}"    # only on machines that set the variable
    optional: true
  - path: .env.local              # gitignored local overrides win
    optional: true
```

### Session-wide env files

A top-level `env_file` takes the same entries and lies beneath every service's own `env_file` and `environment`, so a list every service would repeat is written once:

```yaml
env_file:
  - .env
  - path: .env.local
    optional: true
services:
  api:
    command: "./run-api"
    env_file: api.env       # sees and overrides the session-wide values
```

Its values resolve like a service's env files: against the environment micromux runs in, the files before them, and their own earlier lines, with the [session variables]({{< relref "sessions.md" >}}#injected-variables) winning, so `DATABASE_URL=postgres://127.0.0.1:${POSTGRES_PORT}/app` gets the allocated port. A service sees them in its own env files, its `environment`, and everything after. A [dynamic service]({{< relref "../agent-control/dynamic-services.md" >}}) gets them too, beneath its own `environment`, with the values the session's services got: as the session last loaded its config, at start, on `ctl reconcile`, or before a restart.

The session's own environment includes them too: `micromux env` and `micromux exec` without `--service` read them from disk on every call, and `micromux env --file` writes them to a dotenv file for tools that only read files. See [Run commands in the session environment]({{< relref "../agent-control/control-plane.md" >}}#run-commands-in-the-session-environment).

## Variable interpolation

Every value that reaches the service process may reference variables: the contents of `env_file`, `environment`, `ports`, `command`, and `healthcheck.test`.

| Form | Meaning |
|---|---|
| `${VAR}` or `$VAR` | The value of `VAR`. An error when `VAR` is unset. |
| `${VAR:-default}` | `default` when `VAR` is unset or empty. |
| `${VAR-default}` | `default` only when `VAR` is unset. |
| `$$VAR`, `$${VAR}` | A literal `$VAR` or `${VAR}`, not substituted. |

A `$` is only special directly before `{` or a name. A lone `$$`, `$1`, `$?`, or `$(…)` is left alone.

**What a reference sees.** Values resolve against the environment the process receives (apart from the terminal-color variables micromux sets at spawn): the environment micromux itself runs in, then the [session-wide `env_file`](#session-wide-env-files), then each of the service's `env_file` in order, then `environment`. The [session variables]({{< relref "sessions.md" >}}#injected-variables) (`MICROMUX_SESSION` and every allocated port) count as part of micromux's own environment and override any `env_file` or `environment` definition of the same name. Entries resolve top to bottom, so an `env_file` line can use earlier lines and earlier files, and an `environment` entry can use every env file and only the entries above it. The exception is paths — `working_dir` and every `env_file` path — which only see micromux's own environment, because they have to be resolved before any file can be loaded.

**Unset variables are errors.** micromux never substitutes an empty string on its own. A reference to an unset variable fails validation and startup with a message naming the service, the field, and the variable, and any optional `env_file` that was skipped is reported alongside it as a note. Write `${VAR:-}` to ask for an empty value explicitly. Because `validate_config` runs in its own process, its environment can differ from the session's; an optional `env_file` found on one machine may be skipped on another.

**Substitution happens per argument.** `command` and `healthcheck.test` are split into arguments first, then each argument is substituted, so a value containing spaces stays one argument and no shell is needed to pass it. For the same reason a default that contains spaces has to be quoted:

```yaml
command: 'serve --token "${API_TOKEN}" --name "${NAME:-dev box}"'   # argv: serve, --token, <the token>, --name, dev box
```

**Inside a dotenv file**, single quotes keep a value literal: `PASSWORD='pa$$word'` is taken as written, while unquoted and double-quoted values are interpolated like every other value.

### Shell scripts in `command`

micromux substitutes `$VAR` everywhere, including inside a script handed to `sh -c` or `CMD-SHELL`. Substitution happens once, when the config loads, from micromux's environment, not at run time from the shell's. That has two consequences for an existing script:

- A variable the script defines itself — a loop variable, an assignment, a positional parameter by name — is not set when micromux resolves the command, so it is an error. Escape it with `$$` and the shell keeps it:

  ```yaml
  command: ["sh", "-c", "for f in *; do echo $$f; done"]
  ```

- A variable that *is* set in micromux's environment is substituted before the shell ever runs. `$PWD` becomes micromux's directory, not the service's `working_dir`; write `$$PWD` when you want the shell's value.

The substituted text is also expanded a second time by the shell, which mangles values containing `$`, quotes, or backslashes. Prefer dropping the shell when it exists only to get variables expanded; the per-argument substitution above covers that without a wrapper. Where a shell is genuinely needed, `$${VAR}` hands `${VAR}` to the shell, which expands it from the same environment at run time.

## Ports

`ports` is a list of ports a service uses. micromux does not bind them itself — the list is metadata shown in the UI and used by tooling (for example, freeing ports when stopping a session):

```yaml
ports:
  - 8080
  - 5432
```

## Jobs

Most entries are **services**: long-running processes that micromux keeps up for as long as the session runs. A **job** runs to completion instead, like a migration, a seed, or a code generator. Mark one with `kind: job`:

```yaml
services:
  seed:
    kind: job
    command: ./scripts/seed.sh
    depends_on:
      - name: postgres
        condition: healthy
  api:
    command: ./run-api
    depends_on:
      - seed              # a bare dependency on a job waits for it to complete
```

| State | Meaning |
|---|---|
| `Completed` | The job exited with status 0. The TUI shows it **COMPLETED** and grays it out, like a disabled service. |
| `Failed` | The job exited with another status and will not be retried. The TUI shows **FAILED** with the exit code, in red. |
| `Exited` | A failed attempt that `restart: on-failure` is about to retry (**RETRYING** in the TUI). |

- **Dependencies.** A dependency on a job without a `condition` waits for it to complete. `started` is met while the job runs and after it completed. A job has no health, so `condition: healthy` on a job is an error. A failed job makes its dependents [fail fast]({{< relref "dependencies.md" >}}#dependencies-that-cannot-recover).
- **Restarts.** A job never restarts on its own: it ignores the top-level `restart` default, and may set only `restart: "no"` or `restart: on-failure[:N]`, which retries failures and counts every attempt however long it ran. A job cannot have a `healthcheck`.
- **Running it again.** Restart it, with `r` in the TUI, `micromux ctl restart <id>`, or `restart_service` over MCP; restart-all reruns every job that runs on start. Enabling never starts or cancels a run of a job: a disabled job that finished keeps its outcome, and one that never ran, or that micromux stopped before it finished, is pending again. A run that micromux stopped never counts as completed, even when it exits with status 0.
- **On demand.** `run: manual` keeps a job from running when the session starts; it shows **IDLE** until you restart it, and restart-all leaves it alone. Use it for jobs you run by hand, such as resetting a database. The MCP `ensure_service_ready` runs a manual job that has not run. `run: on-start` is the default, and `run: manual` cannot be combined with `disabled: true`.
- **Waiting.** [`micromux ctl wait`]({{< relref "../agent-control/control-plane.md" >}}#waiting-from-a-script) and the MCP `wait_for_healthy` treat a completed job as ready and a failed one as an exit.

Converting a one-shot service into a job:

```yaml
# before                                # after
seed:                                   seed:
  command: ./seed.sh                      kind: job
  restart: "no"                           command: ./seed.sh
api:                                    api:
  command: ./run-api                      command: ./run-api
  depends_on:                             depends_on:
    - name: seed                            - seed
      condition: completed
```

## Disabling a service

Set `disabled: true` to leave a service **disabled when the session starts**. It won't run until you enable it — from the TUI with `d`, or with `micromux ctl enable <id>`:

```yaml
services:
  worker:
    command: "./run-worker"
    disabled: true
```

## Cleanup

`cleanup` runs a command to completion after each of the service's processes exits, to undo what the service set up outside its own process tree, such as containers that `docker compose up` started:

```yaml
services:
  infra:
    command: docker compose up --menu=false --remove-orphans
    cleanup:
      command: docker compose down --remove-orphans   # same forms and interpolation as `command`
      timeout: 2m                                     # optional; default 60s, at most 1h
```

**When it runs.** After every exit of the service's process: a stop, a restart, a disable, a crash, a clean exit, and the session's own shutdown. It runs once the process and everything micromux stops with it are gone, and before the exit is reported: meanwhile the service shows as stopping, and a restart starts the next run only once the cleanup finished. Only services from the config file can have a cleanup; dynamic services cannot.

**Where it runs.** In the service's environment (its `env_file`s, `environment`, and the [session variables]({{< relref "sessions.md" >}}#injected-variables)) and its `working_dir`, with no input and no terminal.

**Timeout.** A cleanup still running after its `timeout` gets the service's `stop_signal`, and `SIGKILL` 5 seconds later, together with everything it started. Whatever a cleanup leaves running when it exits is stopped at once, so a cleanup cannot start a lasting background process.

**Output.** Each line the cleanup prints appears in the run's log as `[cleanup] <line>`. The output of the latest cleanup is also kept in a file, framed by a line saying when it started and a line saying how it ended: `~/.local/state/micromux/cleanup/<config-key>/<service>.log` on Linux and `~/Library/Application Support/com.romnn.micromux/cleanup/<config-key>/<service>.log` on macOS, where `<config-key>` is a digest of the config file's path. With [`MICROMUX_STATE_DIR`]({{< relref "sessions.md" >}}#isolated-state) set, it is `cleanup/<config-key>/<service>.log` in that directory.

**Status.** The service's timeline records `CleanupStarted`, then `CleanupFinished` or `CleanupFailed` with the exit code or the timeout, and its status carries `last_cleanup`: the run it followed, whether it succeeded, its exit code, and whether it timed out. The MCP `diagnose` tool reports every service whose last cleanup failed. A failure is only reported: it never blocks a restart or the session.

**When micromux dies.** If micromux is killed, `SIGKILL` included, while a service runs, the service's shim stops the service's processes and then runs the cleanup itself. A cleanup micromux had already started keeps running and finishes, bounded by its timeout. A session started again on the same config waits for such cleanups before it starts any service, and says so: `waiting for the cleanup of infra from the previous session (up to 81 s)`. Each shim records how long its stop and cleanup can still take, and the session waits that long, at least the longest stop grace period plus cleanup timeout of the config's services, or the defaults if those are longer; when a cleanup still runs after that, the session fails to start and points to the cleanup's log. The MCP `start_session` tool waits along with the session, for as long as the wait is bounded.

**Exiting at once.** While a session shuts down, micromux names the services whose cleanups it is still waiting for. A second Ctrl-C exits micromux at once, leaving the remaining stops and cleanups to the shims, each still bounded by its stop grace period or timeout.

Known gaps:

- micromux killed in the microseconds between a service's process exiting on its own and micromux noticing runs no cleanup for it.
- A process that exits on its own can leave processes outside its process group running; micromux does not sweep them before the cleanup runs.
- On macOS, processes that a timed-out cleanup detached from its process tree can escape the kill.
- On Windows there is no shim: micromux runs the cleanup itself, and micromux exiting kills a cleanup that is still running.

## Shared resources

Sessions of different worktrees run side by side, but some services should run only once per machine, because each copy costs memory, a GPU, or a fixed port: a desktop app every worktree uses, say. Name such a service's `resource`, and only one session at a time runs it:

```yaml
services:
  desktop:
    command: ./run-desktop-app
    resource: desktop-app      # lowercase letters, digits and inner hyphens, at most 63 characters
```

**Who holds it.**

- A resource is held by at most one session of your user on this host; the rest queue for it, first in, first out.
- Only the session holding the resource starts the services that name it.
- A command that starts such a service, `restart`, `enable`, `restart-all`, or a dynamic service's start or replacement, takes the resource at once, even while the service still waits for its dependencies, as `reserve` would. The session then holds it until it releases it or the grant lapses, and the grant's reason says which command took it, such as "taken by enable desktop".
- While another session holds the resource, the command fails, changes nothing, and names the holder: its session and worktree, its estimate and reason, and when it must renew. A refused `restart-all` keeps the resources it took before the refusal, and its message names them.
- Session startup and other automatic starts take the resource when the service is ready to run. While another session holds it, they disable the service with one event naming the holder. A disabled service does not start on its own once the session gets the resource later: enable it then.

**Reserve, wait, renew, release.** From a session's project:

```bash
micromux ctl reserve desktop-app --for 1h --reason "test the model loader against this worktree's build"
micromux ctl wait --reservation desktop-app --timeout 2h   # renews the queued request while it waits
micromux ctl renew desktop-app                             # lists who waits
micromux ctl release desktop-app
```

`reserve` grants the resource at once when it is free and queues the request otherwise. `--for` and `--reason` are shown to everyone else and change nothing about the grant; a later `reserve` without them keeps the ones given before. Reserving a resource that no service of the session names warns, in case of a typo, and reserves it anyway. After a grant, `reserve` and `wait` name the session's services that need the resource and do not run, with the `micromux ctl enable` command that starts them. The MCP server has the same operations as `reserve_resource`, `wait_for_reservation`, `renew_reservation`, and `release_resource`. See [the control plane]({{< relref "../agent-control/control-plane.md" >}}#shared-resources) for the exit codes.

**Renew while others wait.** A grant runs until its `renew_by` deadline, one hour after it was granted or last renewed. Each renewal lists the queue, so a holder sees the demand at least once an hour. Past the deadline, the holder keeps the resource for as long as nobody waits for it, and a late renewal extends it. Once a request waits, the holder's next check, start, or renewal finds the grant lapsed: the session stops and disables its services that need the resource and passes it to the next request. A queued request lapses at its own deadline, and a running `wait --reservation` renews it.

**Release.** Releasing first stops and disables the session's services that need the resource, then passes it on; until they have stopped, the reservation reads as being released. The next session gets the resource only once none of those processes or their cleanups is left, so it can start its own copy on the same ports. A session that stops gives up its resources once its services have stopped.

**Dead sessions.** When a session's process is gone, through a crash, a kill, or a reboot, the next session that looks drops its grant and queued requests at once, without waiting for the deadline. It gets the resource only once the dead session's services and their cleanups have ended, as after a release.

**Who may control these services.** Only clients working in the session's own project may start, restart, enable, disable, or replace a service that names a resource, type into it in an attached TUI, reserve, renew, or release for the session, apply a reconcile or a reload that touches such a service, or stop the session while it holds, waits for, or runs a resource. Your working directory decides your project: the config found at or above it, whatever `-c` selects. Every other client is refused, with the project that may. The scope is cooperative: it keeps sessions of one user from taking each other's resources by accident, it is no security boundary. The session's own TUI may always; it has no keys to reserve, renew, or release, which are for agents and scripts.

**Changing `resource`.** A config reload or a reconcile refuses to add, remove, or rename the `resource` of a service that runs or was asked to start; stop the service first.

**Rollout.**

- Every worktree that runs the service needs a micromux that understands `resource`: an older one ignores the key with a warning and runs the service regardless. Set `strict: true` so an older micromux refuses the config instead.
- A client older than control protocol 3.21 names no project, so it can no longer control a service that names a resource, or stop a session that uses one; its refusal says to run a newer micromux from inside the project.
- A service with a `resource` needs a config loaded from a file, and process containment, so `resource` is refused on platforms without it, such as Windows.

Known limitations:

- The deadline is kept in wall-clock time between sessions, so a clock step can end a queued request early, or keep a request whose agent is gone queued longer.
- `ctl ls`, `ctl describe`, MCP, and the TUI show the reservation files as last written, so a session that died can show as holder or waiter until any session next reserves, renews, polls, or starts a service that needs the resource.

## Per-service overrides

`restart`, `healthcheck` timing, and `logs` set at the top level are inherited by every service, and any service can override them:

```yaml
restart: unless-stopped        # default for all services
services:
  api:
    command: "./run-api"       # inherits unless-stopped
  migrate:
    command: "./migrate"
    restart: "no"              # one-shot: never restarted
```

See [Dependencies]({{< relref "dependencies.md" >}}), [Healthchecks]({{< relref "healthchecks.md" >}}), [Restart policies]({{< relref "restart-policies.md" >}}), and [Logs]({{< relref "logs.md" >}}) for each of these in depth.
