# CLAUDE.md

Working notes for Luthier, a CLI package manager for FOSS Linux audio
software. Not a DAW: no audio engine, no plugin host, no MIDI, no GUI.

## Commands

```console
cargo test --workspace                     # 427 tests, fully offline
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
cargo run -p luthier-registry-tool -- schema > schemas/package-v1.json   # after type changes
```

Manual run against the bench, which lives in this repository under `bench/`:

```console
cargo run -p luthier-cli -- --root /tmp/luthier-test --registry-path bench \
  --yes install lsp-plugins
```

Always pass `--root` when testing by hand. Without it the binary writes to the
real `~/.clap` and `~/.vst3`.

## Crate map

| Crate | Contains | Must not contain |
|---|---|---|
| `luthier-manifest` | schema types, parse, validate, SPDX, path/hash newtypes, manifest discovery | async, HTTP, *installation* policy |
| `luthier-core` | registry, resolver, download, archive, install, state, scan, signature verification, `api::{Session, Environments}` | anything CLI-shaped |
| `luthier-cli` | `luthier` binary: clap args, rendering | **any business logic** |
| `luthier-registry-tool` | `luthier-registry` binary: validator + authoring helpers | dependency on `luthier-cli` |

`docs/ARCHITECTURE.md` §"What each crate holds" lists every module and what it
carries; this table is only the boundary rules.

`luthier-cli` holding no decisions is a hard rule — a GUI is planned and will call
`api::Session` unchanged. If the CLI needs to decide something, add it to
`luthier-core::api`.

Environment management is `api::Environments`, not `Session`: a session's
`Layout` is already redirected into the selected environment, and `env create`
/ `env remove` act on environments from outside one. `Environments` also owns
the selection itself — `selected_layout()` is what turns `--env`/`LUTHIER_ENV`
into paths, and refuses a name that is not there.

Modules are files, not `mod.rs` directories: `archive.rs` beside `archive/`.
The one exception is `crates/luthier-core/tests/support/mod.rs`, which must
stay a `mod.rs` because cargo compiles every direct child of `tests/` as its
own test target.

`luthier-manifest` staying async-free is load-bearing, and now enforced rather
than asserted: `luthier-registry-tool` has an `authoring` feature, on by
default, carrying everything that needs the network or an archive decoder
(`hash-url`, `inspect`, `check-updates`, `validate --check-urls`). Built with
`--no-default-features` it pulls 70 crates instead of 164 — no tokio, no
reqwest, no zip/tar/7z/xz, no `luthier-core` — and still validates and prints
the schema. That is what registry CI runs on every pull request, and the
manager's own CI builds it that way so the configuration cannot rot.

This is why `manifest_files` lives in `luthier-manifest` rather than
`luthier-core`: validating a tree should not need a runtime to list files.
`luthier-core` re-exports it, so there is still one implementation of the rules
— notably that a symlink in an untrusted snapshot is skipped. Discovery is not
the "filesystem policy" the crate map bans; that means `Layout`, destinations
and ownership, which stay in `luthier-core`.

The same rule applies to `install::derive`, which reads install rules out of an
extracted tree. `luthier-registry inspect` prints what it returns and the
installer will act on it, so there must be exactly one implementation or the
tool's suggestion and the manager's behaviour drift apart — which is how LV2
came to be dropped from every manifest written with `inspect`. It lives in
`luthier-core` because deriving what to install *is* installation policy, and
the crate map keeps that out of `luthier-manifest`.

## Invariants — do not break these

1. **Manifests never name a destination.** An install rule gives `format` +
   `source` (path inside archive) + `kind`; the destination is derived from the
   format's root. A `destination` field would be an arbitrary-write primitive
   for anyone with a merged registry PR. Optional `rename` is a single filename,
   validated as such.
2. **No install scripts, ever.** No field exists to run a command, and none
   should be added.
3. **One extraction policy.** All of it lives in `archive/safe.rs`. Never call
   `tar::Archive::unpack` or `ZipArchive::extract` — they apply their own
   policy. Per-format modules decide only *what entries exist*.
4. **Verify before extract, extract before install.** `Downloader::fetch`
   cannot return a file that failed its checksum. The same order holds one
   level up: `HttpSnapshotRegistry::refresh` checks a bench's signature
   between the download and the extractor, so a snapshot nothing vouched for
   is never opened and every refusal leaves the previous snapshot in place.
5. **`Layout` is injected everywhere.** Nothing deep in the call graph reads
   `$HOME` or calls `dirs::home_dir()`. This is what makes the suite hermetic.
6. **State is authoritative for ownership; scanning is advisory.** Never delete
   a file because its name matches a package.
7. **Determinism.** Resolution branches on package ID, never iteration order
   (`BTreeMap`/`BTreeSet`, sorted edges, releases sorted by version). Lock files
   depend on this later.

## Gotchas found the hard way

- **`Path::components()` normalizes interior `.` away**, so `a/./b` slips past a
  component-based traversal check. Path validation in `luthier-manifest/src/path.rs`
  is deliberately *textual*.
- **Real tarballs are `./`-prefixed.** GNU tar writes `./` entries for
  `tar -C dir .`; Surge XT ships that way. `archive/safe.rs::normalise` strips a
  *leading* `./` only — interior `.` and any `..` stay refused.
- **Packs have no artifacts.** `kind: pack` (and `external`) must skip the
  artifact requirement in `resolver::select_version`, or they fail with
  "no release for <target>".
- **The `zip` crate refuses to write duplicate names**, so that fixture can't be
  built; the reachable collision is `name/` plus `name`.
- **`spdx` rejects deprecated ids at parse time**, so `GPL-3.0` surfaces as
  `InvalidExpression`, not our `Deprecated` variant.
- **Pin `zip` to 8.x** — `cargo search` reports a 9.0.0 prerelease.
- **Pin `sevenz-rust2` to 0.20** — 0.21+ raises its MSRV to 1.93. The library
  dependency takes no default features; the dev-dependency adds `compress`
  because only the test fixtures write archives.
- **DPF-Plugins ships `ProM.clap` as a directory**, not a shared object, so no
  rule is derived for it and `ClapInstaller` would refuse one: relaxing that
  would install something no host is guaranteed to load. Derivation does not
  look *inside* it either — the binary in there sits beside the
  `resources/presets/*.milk` it loads, and installing the two apart is worse
  than installing neither. This is what retired the `dpf-plugins` shadow.
- **MSRV is 1.89** because `File::try_lock` is used instead of an `fs4` dep.
- **A download ceiling is per artifact, not global.** `Downloader::ceiling`
  reads the manifest's `size`; the fixed 4 GiB default applies only when a
  manifest declares none. A global ceiling made CrocellKit (5.26 GiB)
  uninstallable on every machine. `ExtractLimits::for_download` scales the same
  way, by ratio rather than by a fixed figure.
- **A `.part` file survives a failure that says nothing about its bytes.**
  Transport failures and `--offline` keep it so the next *command* resumes;
  anything about the bytes themselves (checksum, size, ceiling) deletes it. See
  `keeps_partial`.
- **`Session::index()` returns a borrow and memoises.** `install` used to build
  the merged index three times. Refresh does not read the index, so there is
  nothing a later call could see that the first did not. The two detection
  scans memoise for the same reason: `install` ran each twice, and a session is
  one command.
- **Every refusal in `remove` happens before the first delete**, and the state
  file is committed per package as `install_at` does. Deleting first and
  checking later left earlier packages gone from disk and still recorded —
  state that disagrees with itself, which `verify` reports as missing files and
  `install` refuses to fix because the package reads as satisfied. Naming a
  package twice is one removal, not two.
- **Removal decides file by file and follows no link.** Keeping a whole
  bundle because one file in it changed left a plugin hosts still loaded, or a
  library no command could reach, since the package was already gone from
  state. `install::remove_entry` deletes what still hashes as installed and
  keeps the rest; `remove_dir_all` never followed a symlink, and deleting file
  by file must not start. `update` refuses up front (`LocalChanges`) rather
  than silently discarding the same edits.
- **A bare file is named by its URL.** The cache knows an artifact only by
  digest, and for `archive = "none"` the published name is what says the file
  is a CLAP. `archive::place` puts it through `SafeExtractor` like any entry
  and sniffs the bytes first; `ArchiveFormat::from_filename` is the one list
  of what may be bare (`.clap`, `.sf2` — never `.vst3`, a directory on Linux).
- **`Path::starts_with` is not containment.** `~/.clap/../x` starts with
  `~/.clap`. `Layout::is_managed_location` requires plain components strictly
  below a root, and never a root itself — roots nest, so that means *any*
  root.
- **A name out of `config.json` is not a validated name.** `bench remove`
  validates before using one as a path segment, because `add_bench` validating
  on the way in proves nothing about an entry that arrived some other way, and
  the delete is recursive: `registries/..` is the data directory.
- **A plan carries its own refusal** (`InstallPlan::blocked` / `refusal()`).
  A front end raises that rather than calling `install` and letting the
  installer re-derive a verdict from freshly-read state; two derivations can
  disagree, and then something nobody confirmed gets installed.
- **`--json` is not consent.** It says how to render an answer. Destructive
  commands need `--yes`, exactly as they do on any non-interactive stdin.
- **A `.part` is never held by a package.** Its digest names what the finished
  file will hash to, so matching it against installed artifacts reports a
  truncated download as in use and keeps `cache clean` from ever collecting it.
- **One bench failing does not fail `refresh`.** Each provider reports for
  itself (`RefreshOutcome::failure`), the snapshot already on disk survives a
  failed fetch, and only *every* bench failing is an error. The default
  configuration lists two, so the old behaviour — `?` on the first provider —
  meant an unpublished or briefly unreachable bench cost the user the one that
  was working.
- **A signature file names its own key, and that is the point.** A first
  fetch has nothing to check against, so the key it carries is pinned exactly
  as the origin is and every later refresh must match it. Verifying a file
  under the key it names is not trust; `signature::check` decides that
  separately, which is why `SignatureFile::verifies` says nothing about it.
- **An unreadable signature is a refusal, not an absence.** Treating a
  malformed `.sig` as "unsigned" would make publishing garbage a way to turn
  verification off.
- **`provenance::record` never lowers protection.** It keeps a previously
  pinned key when a refresh was accepted unsigned, or one `--allow-unsigned`
  would disable verification permanently. `bench untrust` is the deliberate
  way to drop it.
- **Derived rules can only promise what derivation recognises.**
  `install::installable` refuses an artifact whose rules are derived and whose
  `provides` names nothing in `derive::DERIVABLE_FORMATS`, before the download
  rather than after — the Open Audio Stack registry carries standalone
  programs and VST2 builds, and each used to be fetched in full and then
  turned down by the installer. A test keeps the list and `recognise` in step.
- **Content is derived from the archive's shape, not from a name.** A plugin
  announces itself with an extension; a folder of samples does not. So
  `derive::content_of` reads one wrapper directory or none, and
  `install::plan_content` installs it as `<library root>/<package id>` —
  the ID, because the wrapper is named after a commit and changes on every
  release. Shape is reported for *every* tree and acted on only where the
  artifact declares `library`; deriving content whenever no plugin turned up
  would install a broken plugin release as a folder of samples.
- **Only the workspace copy is transient.** `space_needed` charges the cache
  and the install root the plan's total but the workspace only its largest
  single package, because `InstallTransaction::commit` deletes the workspace
  between packages. Charging the total three times refused installs that fit.

## Adding things

- **A plugin format** → implement `FormatInstaller` in `install/formats.rs`, add
  it to `installers()`, add its root to `Layout::default_plugin_roots` and
  `Layout::default_system_roots`, add the variant to `Format`, and list it in
  `validate::INSTALLABLE_FORMATS`. All staging/atomicity/rollback is shared; the
  impl is ~40 lines. `the_validator_and_the_installers_agree` fails until the
  constant and the installer table match, which is the drift this splits risks.
- **A warning a real manifest must be allowed to trip** → add a variant to
  `AllowedWarning` in `luthier-manifest/src/types.rs`, honour it where the
  warning is raised in `validate.rs`, and report the allowance as doing nothing
  when it silences nothing. Never add one that relaxes a check on *where* a
  file is written; these name conventions, not safety.
- **An environment-aware path** → do nothing. Every path comes from `Layout`,
  and `into_env` redirects the per-installation parts. Code that builds a path
  from `$HOME` or from `data_dir()` by hand is the bug.

- **A signing decision** → `registry/signature.rs` holds the policy and the
  file format; `provenance.rs` holds what is pinned. Keep them apart: a
  signature file that verifies under the key it names proves only that the
  file is internally consistent, and whether that key is one to trust is a
  separate question with a separate answer. `--allow-unsigned` covers the
  *absence* of a signature for one run and never discards a pin; nothing
  should ever make it cover a signature that failed.
- **A registry backend** → implement `RegistryProvider` (see `registry/local.rs`)
  and a `RegistrySource` variant in `config.rs`. `registry/oas/` is the worked
  example of a backend whose source has a different schema: every difference
  between the two vocabularies is decided in `oas/translate.rs`, and what the
  source does not carry is marked `derive_install` rather than invented.
  `GitRegistry` via `gix` is the obvious next one.
- **An archive format** → add to `ArchiveFormat`, `SUPPORTED_ARCHIVES`, the
  dispatch in `archive.rs`, the magic table in `sniff_bytes`, **and the
  security corpus**.
- **A package** → see `docs/REGISTRY.md`. Never hand-write a checksum or install
  rule; derive both with `luthier-registry hash-url` and `luthier-registry inspect`.
- **An engine** → `builtin_engines()` in `luthier-manifest/src/engines.rs` when
  it is the reference implementation for its format or otherwise worth every
  build knowing, with a `detect` rule only if the installed name is stable;
  otherwise one `[[engine]]` entry in a bench's `engines.toml`, which merges on
  top. Nothing else in code. A `detect` rule is what finds a copy this manager
  did not install, so it is needed wherever the engine's own manifest carries
  none — everything from OAS. **New content** (a value like `sfz`) → add it to
  `Content` in `types.rs`, give it an engine in `builtin_engines()` (a test
  fails otherwise) and a sentence in `Content::played_by`, then regenerate the
  schema; the OAS translator picks it up from `contains` without further
  change.
- **A category** → add the variant to `Category` in `luthier-manifest/src/types.rs`,
  then regenerate the schema. Validation refuses unknown values, parsing keeps
  them (§7), so an older client reads a newer registry without installing from
  a category it cannot reason about.

## Testing conventions

- Every test builds `Layout::rooted_at(tempdir)`; nothing can reach a real
  plugin directory.
- Artifacts are generated at test time and served over `file://`. No test
  touches the network (§55). Where the behaviour under test *is* HTTP — resume,
  retry, `Range` — the server is a local `wiremock`, which is still not the
  network.
- `crates/luthier-registry-tool/tests/validate.rs` drives the `validate`
  binary the way a reviewer would, including `--strict`. It also validates the
  real `bench/`, unconditionally: that used to be a sibling checkout that might
  be absent, which made the one test covering real data the one most likely not
  to run.
- `crates/luthier-core/tests/archive_security.rs` is the malicious-archive corpus.
  Archives are *generated in code*, not committed as blobs, so a reviewer can
  see what each test feeds the extractor. Every case asserts both the refusal
  and that nothing was written outside the extraction dir. Extend it whenever
  extraction changes.
- `crates/luthier-cli/tests/end_to_end.rs` drives the real binary through the §61
  lifecycle.

## Status

Phases 0–6 of the spec are complete: refresh, search, info, install, list,
verify, update, remove, cleanup, pin/unpin, with dependency resolution and the
hardened extractor.

LV2 and sample libraries install; 7z extracts; a bare CLAP or SoundFont installs; tar hard links are materialised
as copies; `external` detection searches the system plugin directories as well
as the managed roots; environments (`luthier env`, `--env`, `LUTHIER_ENV`)
redirect the per-installation parts of a `Layout`.

A `library` declares `content` (`sfz`, `sf2`, `drumgizmo`);
`builtin_engines()` maps content to engine package IDs, and an `engines.toml`
at a bench's root adds to it — from any registry. Installing
content with no engine present, installed, or in the same plan is refused
before download (`engine::unplayable`, `ResolveError::NoEngine`). This replaced
`requires.toml`, which pinned every SFZ library to sfizz alone. Deliberately
not a dependency: any one engine satisfies it.

Manifests are TOML (`<id>.toml`). Every package declares exactly one
`category` from a closed list (`Category` in `types.rs`, enforced in
`validate.rs`) plus free-form `tags`. `luthier env export` / `env import`
reproduce an installation elsewhere: export pins every version including
dependencies, import feeds them to the resolver as `required_versions`, which
behaves like a pin but is fatal when the version is gone.

`luthier-registry check-updates` reports what upstream has moved past. It reads
the repo from the **artifact URL**, not `repository` — those disagree (Surge XT
publishes from `releases-xt`). Tests point it at a `wiremock` server through the
hidden `--api` flag, so the suite stays offline. GitHub is the only forge, by
decision: nothing in the bench points anywhere else, and SourceForge would mean
inferring a version from a filename (ROADMAP 1.1).

A bench may publish a detached Ed25519 signature at its snapshot's URL with
`.sig` on the end. It is verified before extraction, over the same digest
`provenance.rs` records; the key is pinned on first use, or required from the
first fetch when one is configured (`bench add --key`, `bench trust`,
`bench untrust`). `refresh --allow-unsigned` accepts a missing signature for
one run without discarding the pin. `luthier-registry keygen` / `sign` are the
publishing side, and `docs/REGISTRY.md` carries the rotation procedure.

`luthier cache list` / `cache clean` prune the content-addressed artifact
cache; an entry is kept when some installed package recorded its digest.
`luthier bench list` / `bench add` / `bench remove` edit the registry list in
`config.json` — always the persisted file, never a `--registry-path` override,
so all three describe the same thing. `registry/provenance.rs` pins each
bench's *origin* on first fetch and refuses a silent change of host; the digest
is recorded for audit rather than enforced, because a snapshot's contents
change on every refresh by design.

Deferred, roughly in order of value: macOS and
Windows layouts (the schema and resolver already model them; `Layout` and the
installers are Linux-only); reading OAS's `presets/` and
`projects/` indexes, which is blocked on deciding where a preset installs
given that a manifest may not name a destination; a released binary; aarch64;
a `GitRegistry` backend. A *bench* is the kind —
any collection of manifests, the official one included; `luthier-extras` is
just the default bench's name, as `homebrew-core` names the default tap. It
ships in this repository under `bench/` and is published as the `bench.tar.gz`
release asset — an asset rather than a branch tarball because discovery walks
whatever it is handed (the workspace's `Cargo.toml` files would become
manifests) and because a `.sig` cannot be published under
`/archive/refs/heads/`.
"Registry" in code stays the mechanism (`RegistryProvider`, `RegistryIndex`).

The spec lives in the original task description; section references like §30
throughout the code and docs point at it.
