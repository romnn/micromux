# Agent guidelines

## Rust error handling in tests

Do not scatter imports from `color_eyre::eyre::*` throughout test code. Import
the module once at the top of each test module:

```rust
use color_eyre::eyre;
```

When extension traits or other items are also needed, group them into the same
import:

```rust
use color_eyre::eyre::{self, OptionExt as _, ...};
```

Then qualify macros and other APIs through the module, such as `eyre::eyre!`,
`eyre::bail!`, and `eyre::Result`.

Library code must use typed errors derived with `thiserror`; library crates may
depend on `color-eyre` only as a dev-dependency. Binary crates may use
`color-eyre` as a regular dependency, but production references to it belong
only in the binary's `main` entrypoint. Binary helper modules must also return
typed `thiserror` errors. Test modules are the explicit exception and may use
the dev-only `color_eyre::eyre` re-export described above.

## RAII guard fields

A struct field held only for its destructor (an RAII guard: a lock guard, an
open descriptor that keeps a path valid, a shutdown handle) keeps its
meaningful name plus `#[expect(dead_code, reason = "...")]` stating what the
field keeps alive. Never silence `dead_code` by prefixing such a field with
`_`: underscore fields look inert, hide the RAII intent, and invite deletion in
refactors, while the `expect` reason documents the purpose and flags the
suppression should the field ever gain a real read.

## Changelog

A pull request with a user-facing change (a new feature, a fix users can
notice, or a behavior or breaking change) adds one bullet under `## Unreleased`
in `CHANGELOG.md`: a bold one-line statement, optionally followed by one short
sentence naming the config key, command or flag. CI, tooling, tests, refactors
and dependency bumps get no entry. A release moves the Unreleased entries under
the new `## vX.Y.Z` heading before the bump, which commits `CHANGELOG.md`
through `.bumpversion.toml`.

## Versioning

The Rust library API is not considered stable, so a version bump follows only
what users see: the CLI, the config format and the control protocol. A release
that keeps all three compatible is a patch release, such as 0.4.0 to 0.4.1,
even when it adds features or changes library types; breaking any of them bumps
the minor version. The control protocol stays backward compatible: additions
bump its minor version, and clients send new requests only to sessions that
report it.

## TUI screenshots

Any visual change to the TUI regenerates the documentation screenshots with
`task docs:screenshots` and checks every image in `docs/static/images`: the
header and the footer must each stay on one row. When the footer wraps, raise
`COLS` in `crates/micromux-screenshot/src/scenario.rs` to the narrowest width
that keeps it on one row, plus one column of margin.
