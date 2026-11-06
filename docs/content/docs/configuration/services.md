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

## Variable interpolation

Every value that reaches the service process may reference variables: the contents of `env_file`, `environment`, `ports`, `command`, and `healthcheck.test`.

| Form | Meaning |
|---|---|
| `${VAR}` or `$VAR` | The value of `VAR`. An error when `VAR` is unset. |
| `${VAR:-default}` | `default` when `VAR` is unset or empty. |
| `${VAR-default}` | `default` only when `VAR` is unset. |
| `$$VAR`, `$${VAR}` | A literal `$VAR` or `${VAR}`, not substituted. |

A `$` is only special directly before `{` or a name. A lone `$$`, `$1`, `$?`, or `$(…)` is left alone.

**What a reference sees.** Values resolve against the environment the process receives (apart from the terminal-color variables micromux sets at spawn): the environment micromux itself runs in, then each `env_file` in order, then `environment`. The [session variables]({{< relref "sessions.md" >}}#injected-variables) (`MICROMUX_SESSION` and every allocated port) count as part of micromux's own environment and override any `env_file` or `environment` definition of the same name. Entries resolve top to bottom, so an `env_file` line can use earlier lines and earlier files, and an `environment` entry can use every env file and only the entries above it. The exception is paths — `working_dir` and every `env_file` path — which only see micromux's own environment, because they have to be resolved before any file can be loaded.

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

**Output.** Each line the cleanup prints appears in the run's log as `[cleanup] <line>`. The output of the latest cleanup is also kept in a file, framed by a line saying when it started and a line saying how it ended: `~/.local/state/micromux/cleanup/<config-key>/<service>.log` on Linux and `~/Library/Application Support/com.romnn.micromux/cleanup/<config-key>/<service>.log` on macOS, where `<config-key>` is a digest of the config file's path.

**Status.** The service's timeline records `CleanupStarted`, then `CleanupFinished` or `CleanupFailed` with the exit code or the timeout, and its status carries `last_cleanup`: the run it followed, whether it succeeded, its exit code, and whether it timed out. The MCP `diagnose` tool reports every service whose last cleanup failed. A failure is only reported: it never blocks a restart or the session.

**When micromux dies.** If micromux is killed, `SIGKILL` included, while a service runs, the service's shim stops the service's processes and then runs the cleanup itself. A cleanup micromux had already started keeps running and finishes, bounded by its timeout. A session started again on the same config waits for such cleanups before it starts any service, and says so: `waiting for the cleanup of infra from the previous session`.

**Exiting at once.** While a session shuts down, micromux names the services whose cleanups it is still waiting for. A second Ctrl-C exits micromux at once, leaving the remaining stops and cleanups to the shims, each still bounded by its stop grace period or timeout.

Known gaps:

- micromux killed in the microseconds between a service's process exiting on its own and micromux noticing runs no cleanup for it.
- A process that exits on its own can leave processes outside its process group running; micromux does not sweep them before the cleanup runs.
- On macOS, processes that a timed-out cleanup detached from its process tree can escape the kill.
- On Windows there is no shim: micromux runs the cleanup itself, and micromux exiting kills a cleanup that is still running.

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
