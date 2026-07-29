---
title: Introduction
weight: 1
---

# Introduction

`micromux` is a **local process supervisor with a terminal UI**. You describe your dev services in a `micromux.yaml`, run `micromux`, and it starts them, tracks their state, and gives you one place to watch logs and restart, enable, or disable them.

## The problem

A local dev stack is several long-lived commands at once — an API, a background worker, a database, a frontend dev server. Two ways people usually run them both fall short:

- **A grid of `tmux`/`screen` panes** multiplexes terminals, but knows nothing about your *services*. There's no notion of "start the API only once Postgres is accepting connections", no restart policy, no health, no single status view.
- **Docker Compose** has all of that, but it orchestrates **containers**: images to build, networks and volumes to manage, and a layer of isolation between you and the process. For running scripts and binaries you already have on your machine, that's a lot of overhead.

## The approach

micromux keeps the fast, host-process workflow and adds the service awareness:

- a **structured lifecycle** for every process — pending, blocked, starting, running, healthy, unhealthy, exited, disabled;
- **dependency and health gating** — start a service only after another has started, become *healthy*, or completed;
- **restart policies** — `always`, `unless-stopped`, `on-failure[:N]`, or `no`;
- a **single aggregated UI** to select services, read their (ANSI/interactive) output, and steer them.

{{< figure src="images/overview.png" alt="The micromux TUI showing a sidebar of services in various states beside a live log pane" caption="One view of the whole stack: each service's lifecycle state on the left, the selected service's logs on the right." >}}

## One control plane

Every action — restart, enable, disable, retire — goes through the **same control plane**, whether it comes from a keypress in the TUI, the `micromux ctl` client, or a coding agent over [MCP]({{< relref "agent-control/_index.md" >}}). Because that path respects dependency gating, healthchecks, and restart policy, restarting a service *through* micromux is more correct than `kill` + rerun.

That shared control plane is what makes micromux useful to agents: an agent working in your repo can list services, read crash logs, restart a service, and wait for it to become healthy — driving the exact stack you're watching, with the same semantics.

## The mental model

If you know Docker Compose, the config file will feel familiar — `services`, `depends_on` with conditions, `healthcheck`, `restart`, `ports`, `environment`, `env_file`. The difference is what runs: micromux launches **host processes**, not containers. There are no images, builds, networks, volumes, or isolation. What you gain is speed and directness; what you give up is reproducibility and cross-machine parity. See [How it compares]({{< relref "comparisons.md" >}}).

> [!NOTE]
> **Supported surface.** The CLI and the `micromux.yaml` schema are the supported, stable interface. The published Rust crates exist for micromux's own binaries and integration tests and have no stability guarantees — do not depend on them.
