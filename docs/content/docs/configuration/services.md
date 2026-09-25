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

**What a reference sees.** Values resolve against the environment the process receives (apart from the terminal-color variables micromux sets at spawn): the environment micromux itself runs in, then each `env_file` in order, then `environment`. Entries resolve top to bottom, so an `env_file` line can use earlier lines and earlier files, and an `environment` entry can use every env file and only the entries above it. The exception is paths — `working_dir` and every `env_file` path — which only see micromux's own environment, because they have to be resolved before any file can be loaded.

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
