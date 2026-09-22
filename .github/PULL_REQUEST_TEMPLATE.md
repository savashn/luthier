## What this changes

<!-- What the code does now that it did not before, and why. -->

## Checks

CI gates on all of these, and they are faster to run than to wait for:

- [ ] `cargo test --workspace`
- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo build -p luthier-registry-tool --no-default-features`
- [ ] `cargo run -q -p luthier-registry-tool -- schema | diff -u schemas/package-v1.json -`

The fourth is the one that surprises people: the registry repository builds
the validator without the authoring features on every pull request, so an
import added under the default features breaks a build in another repository.

## If this touches one of these, say how

- **Extraction** — add to the corpus in
  `crates/luthier-core/tests/archive_security.rs`. Every case asserts both
  the refusal and that nothing was written outside the extraction directory.
- **Manifest types** — regenerate `schemas/package-v1.json`.
- **A destination on disk** — every path comes from an injected `Layout`.
  Code that reads `$HOME` or calls `dirs::home_dir()` deep in the call graph
  is the bug, and it is what keeps the test suite hermetic.
- **The CLI** — `luthier-cli` holds no business logic. A decision it makes
  for itself belongs in `luthier_core::api`, where the planned GUI can reach
  it.

## Invariants this does not break

The list is in [CLAUDE.md](../CLAUDE.md) and
[SECURITY.md](../SECURITY.md); the short version is that no manifest can name
a destination or run a command, one policy decides what leaves an archive, and
nothing is verified after it is written.
