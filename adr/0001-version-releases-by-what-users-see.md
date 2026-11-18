# 1. Releases are versioned by the CLI, the config format and the control protocol, not by the library API

**Status:** accepted · **Date:** 2026-11-09

## Context

micromux ships one binary, `micromux`, built from `micromux-cli`.
Users meet three surfaces of it that a release can break: the command line, the `micromux.yaml` config format, and the control protocol that `micromux ctl`, `micromux attach`, `micromux env`, `micromux exec` and `micromux mcp` speak to a running session.

The workspace also publishes its library crates, `micromux`, `micromux-control`, `micromux-tui`, `micromux-mcp` and `micromux-proxy`, to crates.io on every release tag, because `micromux-cli` depends on them and `cargo install micromux-cli` needs them there.
Every crate takes the single workspace version, so a version bump applies to the binary and the libraries alike.
The library APIs are not considered stable: they are the binary's internal layers, and their public types change with ordinary feature work.

Applying Cargo's semver rules to that whole published surface would make every breaking change to a library API, such as removing a public item or changing a public function's signature, a minor release, although no user of the binary can notice it.
The version would then stop telling a user whether an upgrade keeps their scripts, configs and running sessions working.

## Decision

The version follows only what users see: the CLI, the config format and the control protocol.

- A release that keeps all three compatible is a patch release, such as 0.4.0 to 0.4.1, even when it adds features or changes library types.
- A release that breaks any of the three bumps the minor version.
- The library crates make no semver promise of their own; a patch release may change their APIs.
- A control protocol change counts as breaking only when it bumps the protocol's major version; additive changes follow [ADR 2](0002-control-protocol-stays-backward-compatible.md).

One workspace version in `[workspace.package]` of `Cargo.toml` covers every crate.
`.bumpversion.toml` bumps it together with `Cargo.lock` and `CHANGELOG.md` in one commit and tags it, and the tag triggers the release workflow.

Nothing checks the bump mechanically.
The release author judges it from the `## Unreleased` entries in `CHANGELOG.md`, where a breaking change is marked **Breaking:**, as in v0.1.4 and v0.2.0.

## Consequences

- A patch upgrade keeps commands, configs and running sessions working.
- A minor upgrade may need a config edit, a script change, or a restart of running sessions.
- Every release since the rule was written, 0.4.1 through 0.4.5, is a patch release, although each adds features.
- A crate that depends on a micromux library crate from crates.io cannot rely on Cargo's compatible-version resolution and should pin an exact version.
- No test compares the CLI or the config format between releases, so a break that the changelog misses can ship as a patch.
- Revisit the rule when a library crate gains outside users who need a stable API, or at 1.0.

## References

- `AGENTS.md`, sections "Versioning" and "Changelog"
- `.bumpversion.toml`
- `Cargo.toml`, `[workspace.package]`
- `.github/workflows/release.yaml`, which publishes the crates and the binaries on a tag
- `CHANGELOG.md`
- `crates/micromux-cli/src/options.rs`, `VERSION`, which prints the release with the control protocol version
- [ADR 2](0002-control-protocol-stays-backward-compatible.md)
