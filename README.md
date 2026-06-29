# micromux
 
[<img alt="build status" src="https://img.shields.io/github/actions/workflow/status/romnn/micromux/build.yaml?branch=main&label=build">](https://github.com/romnn/micromux/actions/workflows/build.yaml)
[<img alt="test status" src="https://img.shields.io/github/actions/workflow/status/romnn/micromux/test.yaml?branch=main&label=test">](https://github.com/romnn/micromux/actions/workflows/test.yaml)
[![dependency status](https://deps.rs/repo/github/romnn/micromux/status.svg)](https://deps.rs/repo/github/romnn/micromux)
[<img alt="docs.rs" src="https://img.shields.io/docsrs/micromux/latest?label=docs.rs">](https://docs.rs/micromux)
[<img alt="crates.io" src="https://img.shields.io/crates/v/micromux">](https://crates.io/crates/micromux)

 Micromux is a **local process supervisor** with a **terminal UI**.

 Think of it as **Docker Compose for local processes** (not containers) — like **preconfigured tmux panes** that come with Compose-style dependencies, healthchecks, and restarts.

 <p align="center">
   <img src="https://raw.githubusercontent.com/romnn/micromux/main/docs/overview.png" alt="micromux TUI" width="900" />
 </p>

 It runs multiple long-lived commands (your dev “services”) on your machine, tracks their state, and gives you a single place to:
 - **see logs** (ANSI/interactive output supported)
 - **restart/disable services**
 - **gate startup by dependencies + healthchecks**
 - **send input to a service** (PTY-backed “attach” mode)

 <p align="center">
   <img src="https://raw.githubusercontent.com/romnn/micromux/main/docs/healthcheck.png" alt="micromux healthcheck pane showing a failed probe" width="820" />
   <br />
   <sub>Per-service healthcheck pane — a failing probe with its command and output.</sub>
 </p>

 <p align="center">
   <img src="https://raw.githubusercontent.com/romnn/micromux/main/docs/disable.png" alt="micromux with a service disabled" width="820" />
   <br />
   <sub>Disable a service on the fly — it stops and its row turns gray.</sub>
 </p>

 
 ## Install
 
 ```bash
 brew install --cask romnn/tap/micromux

 # Or install from source
 cargo install --locked micromux-cli
 ```
 
 ## Use
 
 Micromux looks for a config in the current directory:
 - `micromux.yaml`
 - `.micromux.yaml`
 - `micromux.yml`
 - `.micromux.yml`
 
 Start it:
 
 ```bash
 micromux
 ```
 
 Or specify a config explicitly:
 
 ```bash
 micromux --config ./micromux.yaml
 ```
 
 Minimal config example:
 
 ```yaml
 # yaml-language-server: $schema=https://github.com/romnn/micromux/raw/main/micromux.schema.json
 version: "1"
 services:
   api:
     command: ["sh", "-c", "./run-api"]
     env_file: ".env"
     restart: unless-stopped
     healthcheck:
       test: ["CMD-SHELL", "curl -fsS http://localhost:8080/health || exit 1"]
       interval: "2s"
       timeout: "1s"
       retries: 10
 
   worker:
     command: "./run-worker"
     depends_on:
       - name: api
         condition: healthy
 ```
 
 TUI controls:
 - **Navigate**: `j`/`k` (or arrows)
 - **Restart**: `r` (current), `R` (all)
 - **Disable/enable**: `d`
 - **Attach mode (send input)**: `a` (exit attach mode with `Alt+Esc`)
 - **Toggle panes/focus**: `Tab`, healthchecks pane: `H`
 - **Logs**: wrap `w`, follow-tail `t`
 - **Quit**: `q` (or `Esc`)
 
 ## Agent control (MCP)

Micromux exposes an **MCP server** so coding agents (Claude Code, Codex) can discover and control your running sessions — list services, read logs, restart/enable/disable them, check health, and wait for a service to become healthy. Actions go through the **same control plane the TUI uses**, so dependency gating, healthchecks, and restart policy are respected — restarting a service via micromux is *more correct* than `kill` + rerun.

Every running `micromux` opens a local, per-project control endpoint (a Unix domain socket under `$XDG_RUNTIME_DIR/micromux/`, same-user only, no network). The MCP server is a thin stdio proxy. Configure it once, like `playwright-mcp`:

**Claude Code** (`.mcp.json`, or `claude mcp add micromux -- micromux mcp`):

```json
{
  "mcpServers": {
    "micromux": { "command": "micromux", "args": ["mcp"] }
  }
}
```

**Codex** (`~/.codex/config.toml`):

```toml
[mcp_servers.micromux]
command = "micromux"
args = ["mcp"]
```

Launched in a project directory, the tools target that project's session automatically. Target another with a `session` argument (`name:<n>`, `pid:<n>`, or `hash:<h>`) or the `MICROMUX_SESSION` env var. Tools: `list_sessions`, `list_services`, `get_logs`, `follow_logs`, `restart_service`, `restart_all`, `enable_service`, `disable_service`, `get_health`, `wait_for_healthy`. `restart_service`/`enable_service` return a run **generation**; pass it to `wait_for_healthy(after_generation=…)` to wait for the *new* run, not the old one.

Name a session so agents can find it by name:

```yaml
name: my-project
```

The control plane is **on by default**; opt out with `--no-control` or `control: { enabled: false }`. Dogfood it from the shell without an agent:

```bash
micromux ctl ls
micromux ctl logs api --tail 50
micromux ctl restart api
```

Build a lean TUI-only binary with the MCP server compiled out via `cargo install --no-default-features micromux-cli`.

 ## How it differs from Docker Compose
 
 Micromux is **not a container orchestrator**.
 - **Runs host processes**: no images, builds, networks, volumes, or container isolation.
 - **Fast local workflow**: start/stop/restart your existing scripts/binaries with a UI.
 - **Dependency + health gating**: delay starting a service until deps are started/healthy/completed.
 
 If you need reproducible environments, networking, volumes, or cross-machine parity, use Docker Compose.
 
 ## How it differs from tmux/screen
 
 tmux/screen are **terminal multiplexers**.
 
 Micromux adds “service awareness”:
 - **Structured service lifecycle** (pending/starting/running/healthy/unhealthy/exited/disabled)
 - **Restart policies** (`always`, `unless-stopped`, `on-failure[:N]`)
 - **Healthchecks and dependency conditions**
 - **Single aggregated UI** for selecting services and viewing logs
