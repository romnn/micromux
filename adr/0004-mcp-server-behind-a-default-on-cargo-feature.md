# 4. The MCP server sits behind the default-on `mcp` feature of `micromux-cli`, so a lean TUI-only build stays possible

**Status:** accepted · **Date:** 2026-06-29

## Context

Coding agents control micromux sessions through an MCP server.
It ships as the `micromux mcp` subcommand of the one `micromux` binary, so a `brew install` or `cargo install` gives agents a server without a second install, and an agent's config is just `{"command": "micromux", "args": ["mcp"]}`.
Its logic lives in the `micromux-mcp` library crate, and the subcommand is a thin shim over it.

`micromux-mcp` builds on the `rmcp` MCP SDK, a heavy dependency that a user who only wants the TUI gains nothing from.
The control plane that the MCP server talks to has no `rmcp` dependency and is useful on its own, through `micromux ctl`, `micromux attach` and `micromux serve`.

## Decision

- `micromux-cli` declares `default = ["mcp"]` and `mcp = ["dep:micromux-mcp"]`, and `micromux-mcp` is an optional dependency.
- Everything MCP-specific in `micromux-cli` is gated with `#[cfg(feature = "mcp")]`: the `mcp` module, the `Command::Mcp` subcommand and its dispatch arm, and the `Error::Mcp` variant.
- The control plane and every other subcommand stay ungated.

A default build, which `cargo install micromux-cli` and the release binaries from `.goreleaser.yaml` produce, includes the MCP server.
`cargo install --no-default-features micromux-cli` builds the TUI, the control plane, `ctl`, `attach`, `serve`, `env`, `exec` and `proxy` without `micromux-mcp` or `rmcp`, and without the `mcp` subcommand.

`cargo fc` builds the workspace in every feature combination, so `task check:fc`, `task lint:fc` and `task test:fc` cover the build without `mcp`.

## Consequences

- Most users get the MCP server without doing anything, and a user who wants a smaller build has one documented flag.
- Code in `micromux-cli` that touches `micromux-mcp` needs the gate, and a missing gate shows up only in the build without `mcp`.
- The MCP server holds no supervision state, so compiling it out takes nothing away from a session.
  It reaches running sessions through the control protocol, and with `--allow-session-start` its `start_session` tool spawns a detached `micromux serve` of the same binary on Unix, a subcommand that exists in either build.

## References

- `crates/micromux-cli/Cargo.toml`, `[features]`
- `crates/micromux-cli/src/main.rs` and `crates/micromux-cli/src/mcp.rs`
- `crates/micromux-cli/src/options.rs`, `Command::Mcp`
- `crates/micromux-mcp/Cargo.toml`, the `rmcp` dependency
- `crates/micromux-mcp/src/tools/sessions.rs`: `spawn_detached_serve`
- `docs/content/docs/installation.md` and `docs/content/docs/agent-control/_index.md`, on the lean build
- `plan/mcp-agent-control.md`, which records the distribution and gating choice
- Commit efef9e7, which added the MCP server with the feature
