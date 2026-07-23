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

## Working directory

By default a service runs in the directory the config was loaded from. Override it with `working_dir` (aliases: `cwd`, `directory`), resolved relative to the config file:

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

`env_file` accepts a single path, an object with `path`, or a list mixing the two. Paths resolve relative to the config file.

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
