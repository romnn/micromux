# 2. The control protocol stays backward compatible: additions bump its minor version, and clients send new requests only to sessions that report it

**Status:** accepted · **Date:** 2026-07-05

## Context

Every running session serves the control protocol, newline-delimited JSON over a local socket.
`micromux ctl`, `micromux env`, `micromux exec`, an attached TUI and `micromux mcp` connect to it as clients.

Sessions are long-lived, so clients and sessions often come from different builds.
A session started before an upgrade keeps running the old binary while every new command uses the new one.
An attached TUI or a `micromux mcp` started before an upgrade is older than the sessions started after it.

Until 2026-07-05 the protocol version was a single integer, and a client refused any session whose version differed.
Every protocol change therefore orphaned the sessions already running, and iterating on micromux kept breaking them.

An older session cannot answer a request it does not know.
Its server fails to parse the frame, answers `BadRequest`, and closes the connection.

## Decision

`PROTOCOL_VERSION` in `crates/micromux-control/src/protocol.rs` is a `ProtocolVersion { major, minor }`.

- An additive change bumps the minor version: a new optional or `#[serde(default)]` field, a new enum variant, or a new request.
- An incompatible change to request or response semantics bumps the major version.
- Peers on the same major line work together.
  `Client::describe` accepts a session on its own major line and returns `ControlError::ProtocolMismatch` for any other.
- Payloads tolerate a peer from another minor revision.
  Additive fields deserialize with `#[serde(default)]`, and a wire enum that can grow ends in a `#[serde(other)] Unknown` variant, so a newer peer's value reads as `Unknown` instead of failing to parse.
  `ErrorCode`, `Health`, `Desired`, `Execution`, `ChangeKind`, `ServiceEventKind`, `OriginKind`, `RetiredReason`, `DependencyFailureReason` and `OutputStream` have one.
- A request added in a minor revision gets a named constant for the first revision whose sessions answer it, such as `EVENTS_PROTOCOL_VERSION` (3.3), `ENVIRONMENT_PROTOCOL_VERSION` (3.15), `TERMINAL_PROTOCOL_VERSION` (3.18) and `SESSION_ENV_FILE_PROTOCOL_VERSION` (3.20) on the 3.x line.
  A major bump retires the constants of the line it leaves, since every peer of the new line answers every request it inherits; 4.0 did.
- A client sends such a request only when the session's reported `SessionInfo::protocol_version` `supports` that revision.
  Otherwise it falls back to an older request, leaves the feature off, or refuses with a message that asks the user to restart the session.

The session reports its version in its `Describe` answer, and the client adapts to it.
The session learns a client's version only from `SubscribeWithVersion`, which picks a subscription behavior both sides understand.

"Protocol compatibility" in `docs/content/docs/agent-control/control-plane.md` describes the revisions from 3.8 on, and `micromux --version` prints the protocol version next to the release.
Unit tests in `protocol.rs` pin each named revision gate; on the 3.x line there was one per revision, such as `events_require_protocol_3_3_on_the_same_major_line`.

## Consequences

- Where the rule holds, an upgrade does not orphan running sessions: a newer `ctl`, `env`, `exec` or MCP server keeps working against an older session on the same major line, with the newer requests switched off.
- Every new request costs a revision constant, a gate in each client that sends it, a fallback or a refusal, and an entry in the compatibility docs.
- New fields must be optional or defaulted, and a new enum variant must be harmless to read as `Unknown`.
- An older client sees newer states only as `Unknown`, so the docs ask users to restart an attached TUI or a running `micromux mcp` after an upgrade that adds states, as 3.16 did for jobs.
- A major bump still orphans every running session.
  It happened twice on 2026-07-06, to 2.0 when `HealthAttempt.run_generation` became required and to 3.0 when `open_ports` was renamed, and on 2026-11-23, to 4.0 when `Enable`, `Reserve` and `ReleaseReservation` changed meaning ([ADR 7](0007-reservations-are-requests-to-run.md)).
- The gates are a convention that review keeps; nothing stops a client from sending a new request without one.

The implementation falls short of the rule in known places:

- Several enums inside wire payloads have no `Unknown` variant, so a value added to them would fail to parse in an older peer.
  `ServiceKind`, `StopSignal` and `DependencyCondition` appear in `ServiceSnapshot` and `DependencyFailure`, `ReconcileActionKind` in reconcile receipts, and `RoutingStatus` in the session summary of a description.
- The compatibility docs leave out the revisions before 3.8 and revision 3.10.
- A protocol major bump is a breaking change in the sense of [ADR 1](0001-version-releases-by-what-users-see.md); a minor bump is not.

The shared proxy's registration protocol in `crates/micromux-proxy/src/protocol.rs` is versioned separately, as an integer revision.
It follows the same idea: every proxy accepts older sessions, and a session whose routes need a newer revision asks the proxy for its revision first and refuses one that is too old.

## References

- `crates/micromux-control/src/protocol.rs`: `PROTOCOL_VERSION`, `ProtocolVersion::is_compatible_with`, `ProtocolVersion::supports`, and their tests; the 4.x line has no revision constant yet
- `crates/micromux-control/src/client.rs`: `Client::describe` and the versioned subscription
- `crates/micromux-control/src/server/unix.rs`: `handle_connection`, which rejects an unknown request with `BadRequest`
- `docs/content/docs/agent-control/control-plane.md`, "Protocol compatibility"
- `docs/content/docs/installation.md`, on `micromux --version`
- `AGENTS.md`, section "Versioning"
- Commit 2371802, which introduced the major and minor version
