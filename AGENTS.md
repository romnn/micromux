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
