---
title: Documentation
bookToc: false
bookFlatSection: false
---

# Documentation

`micromux` is a local process supervisor with a terminal UI. It runs your dev services as host processes, tracks their lifecycle, gates their startup on dependencies and healthchecks, and exposes one control plane that both the TUI and coding agents drive.

## Start here

- **[Introduction]({{< relref "introduction.md" >}})** — what micromux is, and how it thinks about services.
- **[Installation]({{< relref "installation.md" >}})** — install the `micromux` binary.
- **[Quick start]({{< relref "quick-start.md" >}})** — a first config, a first run, and reading the UI.

## Go deeper

- **[Configuration]({{< relref "configuration/_index.md" >}})** — the config file, services, dependencies, healthchecks, restart policies, and logs.
- **[The terminal UI]({{< relref "tui.md" >}})** — panes, keybindings, sending input, and attaching to a running session.
- **[Agent control]({{< relref "agent-control/_index.md" >}})** — the MCP server, the `micromux ctl` client, and runtime (dynamic) services.
- **[How it compares]({{< relref "comparisons.md" >}})** — micromux next to Docker Compose and tmux.

> [!NOTE]
> The **CLI and config file are the supported interface.** micromux also publishes Rust crates, but their API exists for the project's own binaries and integration tests and carries no stability guarantees.

**Platform support.** micromux is developed and fully supported on **Linux and macOS**. On **Windows** the TUI runs, but the control plane (`attach`, `ctl`, `mcp`, `serve`) is not yet available and service termination is not graceful.
