---
title: How it compares
weight: 7
---

# How it compares

micromux sits between two familiar tools. Knowing where it differs makes it clear when to reach for it.

## versus Docker Compose

micromux borrows Compose's config shape — `services`, `depends_on` with conditions, `healthcheck`, `restart`, `ports`, `environment`, `env_file` — but it is **not a container orchestrator**.

- **Runs host processes.** No images, builds, networks, volumes, or container isolation. Your services are the scripts and binaries already on your machine.
- **Fast local workflow.** Start, stop, and restart your existing commands with a UI, no build step in the loop.
- **Dependency + health gating.** Delay a service until its dependencies are started, healthy, or completed — the part a plain process runner lacks.

If you need reproducible environments, networking, volumes, or cross-machine parity, use Docker Compose. If you want to run the processes you already have, quickly and with service awareness, use micromux.

## versus tmux / screen

tmux and screen are **terminal multiplexers**: they tile terminals but know nothing about your services. micromux adds the service layer on top:

- a **structured lifecycle** — pending, starting, running, healthy, unhealthy, exited, disabled — instead of "a shell that may or may not still be running a thing";
- **restart policies** (`always`, `unless-stopped`, `on-failure[:N]`, `no`);
- **healthchecks and dependency conditions**;
- a **single aggregated UI** for selecting services and reading logs, plus retained crash logs across restarts.

Think of it as preconfigured panes that come with Compose-style dependencies, healthchecks, and restarts — and one control plane that [coding agents can drive]({{< relref "agent-control/_index.md" >}}).

## At a glance

| | tmux / screen | **micromux** | Docker Compose |
|---|---|---|---|
| Runs | terminals | host processes | containers |
| Service lifecycle | ✗ | ✓ | ✓ |
| Dependency / health gating | ✗ | ✓ | ✓ |
| Restart policies | ✗ | ✓ | ✓ |
| Images / networks / volumes | ✗ | ✗ | ✓ |
| Reproducible / cross-machine | ✗ | ✗ | ✓ |
| Startup overhead | none | none | build / pull |
| Agent (MCP) control | ✗ | ✓ | ✗ |
