---
title: Installation
weight: 2
---

# Installation

micromux ships a single binary, `micromux`. It is developed and fully supported on **Linux and macOS**; on Windows the TUI runs but the control plane does not (see [platform support](#platform-support)).

## Homebrew

The quickest way on macOS or Linux:

```bash
brew install --cask romnn/tap/micromux
```

## From source

With a [Rust toolchain](https://rustup.rs) installed:

```bash
cargo install --locked micromux-cli
```

This builds the full binary, including the MCP server. For a lean, TUI-only build with the MCP server compiled out:

```bash
cargo install --locked --no-default-features micromux-cli
```

## Verify

```bash
micromux --version
micromux --help
```

`micromux --help` lists the subcommands:

{{< terminal name="help" >}}

With no subcommand, `micromux` runs the TUI for the current project. The others — [`attach`]({{< relref "tui.md" >}}#attach-to-a-running-session), [`ctl`]({{< relref "agent-control/control-plane.md" >}}), [`serve`]({{< relref "agent-control/_index.md" >}}), and [`mcp`]({{< relref "agent-control/_index.md" >}}) — are covered under the relevant sections.

## Platform support

| Platform | TUI | Control plane (`attach` / `ctl` / `mcp` / `serve`) |
|---|---|---|
| Linux | ✅ | ✅ |
| macOS | ✅ | ✅ |
| Windows | ✅ | ❌ not yet |

On Windows the TUI runs, but the control plane is not yet available, and stopping a service kills it immediately without a graceful-termination phase. Windows named-pipe support is planned.

Next: the [Quick start]({{< relref "quick-start.md" >}}).
