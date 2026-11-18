# 5. The shared proxy and `routes` are Unix-only, like the control plane they build on

**Status:** accepted · **Date:** 2026-11-06

## Context

`routes` give a session's services hostnames such as `app.<session>.localhost`.
One shared per-user HTTP proxy, the `micromux proxy` subcommand from `micromux-proxy`, serves them, and the first session with routes starts it.

The proxy builds on Unix-only pieces:

- Sessions register their routes over a Unix domain socket and hold the connection open as a lease (`UnixListener` in `server.rs`, `UnixStream` in `client.rs`).
- That socket and the proxy's lock live in the control plane's coordination directory, `micromux_control::coordination_dir`, which exists only under `cfg(unix)`.
  It is `/tmp/micromux-<uid>`, or the `coordination` directory inside `MICROMUX_STATE_DIR` when that variable sets an isolated state directory.
- A session launches the proxy detached in its own process group, with `process_group(0)`, so it outlives the session that started it.
- The proxy stops on `SIGTERM` and ignores `SIGHUP`, so a closing terminal does not take down routes that other sessions rely on.

The control plane has no Windows transport either.
On non-Unix targets `transport_supported()` is false, and the control client and server return `Unsupported`.
`plan/mcp-agent-control.md` chose a closed set of local transports with no network fallback, and gated the control plane off on Windows until named pipes land as their own milestone; `ControlEndpoint::WindowsNamedPipe` reserves that slot.
The docs say Windows named-pipe support is planned.

micromux still builds, is tested and ships for Windows, where the TUI runs.

## Decision

- `micromux-proxy` compiles its implementation only under `cfg(unix)`; on other platforms only its `protocol` types exist.
  Its runtime dependencies sit under `[target.'cfg(unix)'.dependencies]`, except `serde` and `tracing`, which it declares for every platform.
- In `crates/micromux-cli/src/proxy.rs`, the non-Unix `latch_port`, `register` and `report_to` do nothing, and `micromux proxy` fails with "the micromux proxy is not supported on this platform".
- On Windows a non-empty `routes` list is a config error, "routes are not supported on Windows, which has no micromux proxy", so a config with routes fails to load instead of starting without them.
- The docs state the limit: `routes` in the config reference and in the sessions page, and the platform support notes in the README and the docs.

The cargo-fc target list includes `x86_64-pc-windows-gnu`, so `task check:fc` and `task lint:fc` build the non-Unix paths.
The test workflow runs on `windows-latest`, where `rejects_invalid_routes` checks the Windows rejection.

## Consequences

- On Windows, services run and are reached at `127.0.0.1:<port>`, but get no hostnames.
- A config that uses `routes` does not load on Windows, so a project shared with Windows users must leave routes out.
- The proxy shares the control plane's platform support, so the two stay consistent.
- Revisit this when the control plane gains a Windows transport.
  The proxy then needs a registration channel and a coordination directory on Windows, a detached launch, and a replacement for its Unix signal handling.

## References

- `crates/micromux-proxy/src/lib.rs`, `client.rs`, `server.rs` and `protocol.rs`
- `crates/micromux-proxy/Cargo.toml`
- `crates/micromux-cli/src/proxy.rs`
- `crates/micromux-control/src/endpoint.rs`: `coordination_dir`, `transport_supported` and `ControlEndpoint`
- `crates/micromux/src/config/v1.rs`: the `routes` parser and the test `rejects_invalid_routes`
- `docs/content/docs/configuration/reference.md` and `docs/content/docs/configuration/sessions.md`, on `routes`
- `docs/content/docs/installation.md`, "Platform support", and `README.md`
- `plan/mcp-agent-control.md`, on the closed transport set and Windows gating
- Commit de74aa0, which added the proxy and the Windows rejection
