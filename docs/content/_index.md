---
title: micromux
type: docs
bookToc: false
---

<div class="mm-hero">
  <div class="mm-hero__text">
    <h1>micromux</h1>
    <p class="mm-hero__lead">A local process supervisor with a terminal UI — <strong>Docker Compose for local processes</strong>, not containers. It runs your dev services with dependency gating, healthchecks, and restart policies, and lets coding agents drive the same session over MCP. Open source, MIT-licensed.</p>
    <div class="mm-hero__cmd">micromux</div>
    <div class="mm-hero__actions">
      <a class="mm-btn mm-btn--primary" href="{{< relref "/docs/introduction.md" >}}">Read the docs</a>
      <a class="mm-btn" href="https://github.com/romnn/micromux">Source on GitHub</a>
    </div>
  </div>
  <div class="mm-hero__shot">
    {{< img src="images/overview.png" alt="The micromux TUI: a service sidebar with per-service lifecycle state, next to a live log pane" >}}
  </div>
</div>

<div class="mm-badges">

[![build status](https://img.shields.io/github/actions/workflow/status/romnn/micromux/build.yaml?branch=main&label=build)](https://github.com/romnn/micromux/actions/workflows/build.yaml)
[![test status](https://img.shields.io/github/actions/workflow/status/romnn/micromux/test.yaml?branch=main&label=test)](https://github.com/romnn/micromux/actions/workflows/test.yaml)
[![crates.io](https://img.shields.io/crates/v/micromux)](https://crates.io/crates/micromux)
[![docs.rs](https://img.shields.io/docsrs/micromux/latest?label=docs.rs)](https://docs.rs/micromux)

</div>

## What it does

A dev stack is rarely one process. It's an API, a worker, a database, a frontend — each a long-lived command, some depending on others being *ready*, not merely started. Running them as a wall of `tmux` panes loses that structure; reaching for Docker Compose brings images, networks, and volumes you don't want for local work.

`micromux` runs those commands as host processes and gives them service awareness: a structured lifecycle, dependency and health gating, restart policies, and a single terminal UI to watch and steer them. Because every action goes through one control plane, coding agents can drive the exact same session over MCP.

<div class="mm-cards">
  <div class="mm-card">
    <h3>Service lifecycle</h3>
    <p>Each process has a tracked state — pending, starting, running, healthy, unhealthy, exited, disabled — shown in one aggregated sidebar.</p>
  </div>
  <div class="mm-card">
    <h3>Dependencies &amp; health</h3>
    <p>Gate startup on another service being started, <em>healthy</em>, or completed. Compose-style <code>healthcheck</code> probes with timing and retries.</p>
  </div>
  <div class="mm-card">
    <h3>Restart policies</h3>
    <p><code>always</code>, <code>unless-stopped</code>, <code>on-failure[:N]</code>, or <code>no</code> — set globally and overridden per service.</p>
  </div>
  <div class="mm-card">
    <h3>Agent control (MCP)</h3>
    <p>An MCP server exposes the same control plane the TUI uses, so agents list services, read logs, and restart through supervised semantics.</p>
  </div>
</div>

## Example

```yaml
# micromux.yaml
version: "1"
services:
  api:
    command: ["sh", "-c", "./run-api"]
    ports: [8080]
    depends_on:
      - name: postgres
        condition: healthy
    healthcheck:
      test: ["CMD-SHELL", "curl -fsS http://localhost:8080/health || exit 1"]

  postgres:
    command: "postgres -D ./pgdata"
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -q"]
```

```bash
# Install
brew install --cask romnn/tap/micromux   # or: cargo install --locked micromux-cli

# Run the stack from a directory containing micromux.yaml
micromux
```

## Documentation

- [Introduction]({{< relref "/docs/introduction.md" >}}) and [Installation]({{< relref "/docs/installation.md" >}}).
- [Quick start]({{< relref "/docs/quick-start.md" >}}) — a first config, a first run, and how to read the UI.
- [Configuration]({{< relref "/docs/configuration/_index.md" >}}) — services, dependencies, healthchecks, restarts, and logs.
- [The terminal UI]({{< relref "/docs/tui.md" >}}) — panes, keys, sending input, and attaching to a running session.
- [Agent control]({{< relref "/docs/agent-control/_index.md" >}}) — the MCP server, the control plane, and runtime services.
