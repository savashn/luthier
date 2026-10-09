# Contributing

## Getting started

You need Rust 1.93 or newer, and cmake and perl for `aws-lc-sys`, which the
TLS stack builds from C. With Nix, the flake has all of it:

```console
$ nix develop            # or, with direnv: direnv allow
```

Then:

```console
$ cargo build
$ cargo test --workspace
```

Everything runs offline. No test contacts the network or touches a real plugin
directory; artifacts are generated at test time and served over `file://`, and
every test confines its paths to a temporary root.

CI gates on four more things, all of which are worth running before you push:

```console
$ cargo fmt --all --check
$ cargo clippy --workspace --all-targets -- -D warnings
$ cargo build -p luthier-registry-tool --no-default-features
$ cargo run -q -p luthier-registry-tool -- schema | diff -u schemas/package-v1.json -
```

The third is the one that surprises people. `luthier-registry-tool` has an
`authoring` feature, on by default, carrying everything that needs the network
or an archive decoder. Without it the validator builds from
`luthier-manifest` alone, which is what keeps that crate free of async and
HTTP; nothing but this build notices when an import added under the default
features breaks that.

Three more run in CI without needing anything from you, and are worth knowing
about when they fail:

- **MSRV.** `cargo check` with Rust 1.93, the version `Cargo.toml` promises.
  A std API newer than that fails here and nowhere else.
- **`cargo deny check`**, per `deny.toml`: advisories, licences and sources of
  every dependency. It also runs weekly, so it can fail on a lock file nobody
  touched when an advisory is published against it.
- **`nix fmt -- --ci`** and `nix flake check`, when a Nix file or the
  workspace changes. `nix flake check` builds the package, which runs the
  suite again in the sandbox.

## Layout

```
crates/
  luthier-manifest/       schema, parsing, validation. No async, no network.
  luthier-core/           registry, resolver, downloader, installer, state
  luthier-cli/            the `luthier` binary — arguments and rendering only
  luthier-registry-tool/  the `luthier-registry` validator and authoring helpers
extras/              Luthier's own manifests (extras), MIT-licensed data
schemas/             generated JSON Schema, committed
nix/                 the flake's package and Home Manager module
docs/
```

Business logic belongs in `luthier-core`, never in `luthier-cli`. A GUI is planned and
will call the same API, so anything the CLI decides for itself is a bug.

## Tests

Two things carry more weight than the rest:

**The security corpus** (`crates/luthier-core/tests/archive_security.rs`) builds
malicious archives and asserts they are refused *and* that nothing was written
outside the extraction directory. Archives are generated in code rather than
committed as binaries, so a reviewer can see exactly what each test feeds the
extractor. Add to it when you touch extraction.

**The lifecycle tests** (`crates/luthier-cli/tests/end_to_end.rs`) run the real
binary through install, list, verify and remove against a generated registry.

If you change the manifest types, regenerate the schema:

```console
$ cargo run -p luthier-registry-tool -- schema > schemas/package-v1.json
```

A test fails if you forget.

## Style

Match the surrounding code. Beyond that:

- Explicit error types with actionable messages. An error should say what was
  being done, to what, and what the user can do about it.
- No `unsafe`; both library crates set `#![forbid(unsafe_code)]`.
- Paths come from an injected `Layout`. Nothing deep in the call graph should
  read `$HOME` or call `dirs::home_dir()` — that is what keeps the suite
  hermetic.
- Comment why, not what, and only where the reason is not obvious from the code.

## Adding a package

Packages live under `extras/` in this repository. See [docs/REGISTRY.md](docs/REGISTRY.md).
