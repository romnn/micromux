# 3. Library crates return typed `thiserror` errors, and `color-eyre` appears in production only in the binary's `main`

**Status:** accepted · **Date:** 2026-07-24

## Context

The workspace has five library crates, `micromux`, `micromux-control`, `micromux-tui`, `micromux-mcp` and `micromux-proxy`, and three binary crates: `micromux-cli`, which builds `micromux`, and the unpublished tools `micromux-screenshot` and `clippy-wrapper`.
The libraries are published to crates.io and consumed by the binary.

Callers branch on the kind of failure, not only on its message.
`main` turns `DiagnosticsEmitted`, a cancelled cleanup wait, a cancelled proxy start and `Exit` into exit codes of their own, `session_env.rs` matches `ErrorCode::UnknownService` to explain a wrong `--service`, and `micromux-mcp` maps `ControlError` and `ErrorCode` variants to MCP tool errors.
An `eyre::Report` offers only a downcast for that.

Until 2026-07-24, `micromux`, `micromux-tui` and `micromux-screenshot` depended on `color-eyre` as a regular dependency, and helpers in `micromux-cli` returned `eyre::Result`.
Commit 3e9a7c4 replaced those with typed errors, and commit fb3d9a9 wrote the rule into `AGENTS.md` the same day.

## Decision

- Library code returns typed errors derived with `thiserror`.
- A library crate lists `color-eyre` only under `[dev-dependencies]`.
- A binary crate may list `color-eyre` as a regular dependency, but its production code names it only in the `main` entrypoint.
  In `micromux-cli` that is `main` and `async_main` in `src/main.rs`, which install the report handler and return `color_eyre::Result`.
- Binary helper modules return typed `thiserror` errors too, either the crate's `Error` enum or one of their own, such as `logging::Error` from `logging::setup` in `micromux-cli`.
- Test modules may use `color_eyre::eyre`, imported once at the top of each test module as `use color_eyre::eyre;`, or grouped as `use color_eyre::eyre::{self, OptionExt as _};`.
  They then qualify its items through the module, as `eyre::Result`, `eyre::eyre!` and `eyre::bail!`.

The manifests enforce the library half: since `color-eyre` is only a dev-dependency there, non-test library code that names it does not compile.
The binary half and the test import style are kept by review.

## Consequences

- Callers can match a library failure and choose an exit code, a message or an MCP error from its variant.
- Errors that reach `main` unmatched still print as a `color-eyre` report with its context and backtrace hint.
- Every new failure in a library needs a variant, or a variant that wraps the source error, instead of an ad hoc message.
- A binary helper that wants a quick `eyre!` has to add a variant to a typed error enum instead.
- One library function falls short of the rule: `micromux_mcp::serve_stdio` returns `Box<dyn std::error::Error + Send + Sync>`, which `micromux-cli` wraps unmatched in `Error::Mcp`.
- Tests read uniformly, with every `eyre` item visible as `eyre::…` at its use.

## References

- `AGENTS.md`, section "Rust error handling in tests"
- `crates/micromux-cli/src/main.rs`: the `Error` enum, `main` and `async_main`
- The `[dependencies]` and `[dev-dependencies]` tables of every `crates/*/Cargo.toml`
- `crates/micromux-control/src/lib.rs`: `ControlError`
- `crates/micromux-cli/src/logging.rs`: `logging::Error`
- `crates/micromux-mcp/src/lib.rs`: `serve_stdio`
- Commits fb3d9a9 and 3e9a7c4
