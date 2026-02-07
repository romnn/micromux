# micromux
 
[<img alt="build status" src="https://img.shields.io/github/actions/workflow/status/romnn/micromux/build.yaml?branch=main&label=build">](https://github.com/romnn/micromux/actions/workflows/build.yaml)
[<img alt="test status" src="https://img.shields.io/github/actions/workflow/status/romnn/micromux/test.yaml?branch=main&label=test">](https://github.com/romnn/micromux/actions/workflows/test.yaml)
[![dependency status](https://deps.rs/repo/github/romnn/micromux/status.svg)](https://deps.rs/repo/github/romnn/micromux)
[<img alt="docs.rs" src="https://img.shields.io/docsrs/micromux/latest?label=docs.rs">](https://docs.rs/micromux)
[<img alt="crates.io" src="https://img.shields.io/crates/v/micromux">](https://crates.io/crates/micromux)

 Micromux is a **local process supervisor** with a **terminal UI**.

 Think of it as **Docker Compose for local processes** (not containers) — like **preconfigured tmux panes** that come with Compose-style dependencies, healthchecks, and restarts.

 <p align="center">
   <img src="https://raw.githubusercontent.com/romnn/micromux/main/screenshot.png" alt="micromux TUI screenshot" width="800" />
 </p>

 It runs multiple long-lived commands (your dev “services”) on your machine, tracks their state, and gives you a single place to:
 - **see logs** (ANSI/interactive output supported)
 - **restart/disable services**
 - **gate startup by dependencies + healthchecks**
 - **send input to a service** (PTY-backed “attach” mode)

 
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
