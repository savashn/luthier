# Architecture

## Shape

```
      luthier-cli (luthier)        luthier-registry-tool (luthier-registry)
              │                              │
              ▼                              ▼
        luthier-core  ─────────────────►  luthier-manifest
  registry · resolver · download          schema · parse · validate
  archive · install · state · scan        (no async, no network)
```

Four crates, not the nine the specification sketched. The split is load-bearing
rather than decorative:

- **`luthier-manifest`** has no async runtime and no HTTP stack, because the registry
  validator runs in CI on every pull request and should not need one.
- **`luthier-core`** holds every decision about packages. `luthier-cli` contains none, so
  a GUI can reuse it unchanged.
- **`luthier-registry-tool`** is separate so a bench's CI can build the validator
  without pulling in the CLI.

Splitting `luthier-core` further — a crate each for resolver, downloader, installer —
was considered and rejected: they share the error and manifest types, and the
boundaries would be notional. They are modules.

The first of those claims is enforced rather than asserted.
`luthier-registry-tool` has an `authoring` feature, on by default, carrying
everything that needs the network or an archive decoder — `hash-url`,
`inspect`, `check-updates` and `validate --check-urls`. Built with
`--no-default-features` it pulls 70 crates instead of 164, with no tokio, no
reqwest, no archive decoders and no `luthier-core`, and still validates a tree
and prints the schema. The manager's CI builds it that way on every push, so
the configuration cannot rot unnoticed now that the bench lives in this
repository and nothing else builds it.

That is also why manifest discovery (`manifest_files`) lives in
`luthier-manifest`: validating a tree should not need a runtime to list files.
`luthier-core` re-exports it, so there is one implementation of the rules —
notably that a symlink in an untrusted snapshot is skipped. Discovery is not
the filesystem policy the crate split keeps out of `luthier-manifest`; that
means `Layout`, destinations and ownership, which stay in `luthier-core`.

## What each crate holds

### `luthier-manifest` — the schema layer

Types, parsing, validation. No async runtime, no HTTP stack, no installation
policy. Everything a registry's CI needs and nothing it does not.

| Module | Holds |
|---|---|
| `types` | The value vocabulary: `Format`, `Content`, `Category`, `PackageKind`, `ArchiveFormat`, `Os`/`Arch`/`Target`, `AllowedWarning`, `EntryKind` |
| `manifest` | The v1 manifest: package, release, artifact, install rule, detect rule |
| `parse` | Reading TOML, strictly for CI and leniently for clients |
| `validate` | Every semantic rule, plus `INSTALLABLE_FORMATS` and `SUPPORTED_ARCHIVES` |
| `id`, `path`, `hash`, `license` | Constrained newtypes: a `PackageId` is one path segment, an `ArchivePath` is checked textually, a `Sha256Hash` is 64 hex characters, a licence is a parsed SPDX expression |
| `engines` | `engines.toml` and the built-in list: which packages play which content |
| `discover` | `manifest_files`, the walk that finds manifests in a tree |
| `macros` | `string_enum!`, which gives every string enum its `Other(String)` arm |

### `luthier-core` — every decision

One library behind two binaries and, later, a GUI. `api` is the surface they
call; the rest is one pipeline, from a request to a recorded install.

| Module | Holds |
|---|---|
| `api` | `Session` — refresh, search, info, plan, install, remove, verify, update, cleanup, cache, benches, pins, export/import, and `--prune`'s convergence |
| `registry` | Merging the bench and the Open Audio Stack registry into one `RegistryIndex`, bench first; `local`, `http` and `oas` providers, and refreshing the OAS index once a day, conditionally; `provenance`, a record of each fetch and of when the source was last asked |
| `resolver` | A request and an index into an ordered, deterministic plan |
| `download` | Fetching with a streamed SHA-256, resume, per-artifact ceilings, and the rules about when a `.part` survives |
| `archive` | Opening untrusted containers: `safe` is the single extraction policy, the per-format modules only say what entries exist |
| `install` | Journalled, atomic placement (`InstallTransaction`), per-format installers, and `derive`, which reads rules out of a verified tree |
| `state` | What is installed and which files belong to it — the authority on ownership |
| `scan` | What is present on the system but not installed by Luthier |
| `selfupdate` | `update --self`: the latest release from GitHub's API, how the running binary was installed, and replacing it or handing the package to apt, dnf or zypper |
| `layout` | Every path, injected rather than computed from `$HOME`; the search path a relocated plugin root needs |
| `envfile` | The portable file `export` writes and `import` reads |
| `engine` | Whether anything on the machine can play the content about to be installed |
| `config` | The persisted bench list and `RegistrySource` |
| `error`, `fsutil` | The error model with exit codes and hints; the filesystem primitives the state store and installer share |

### `luthier-cli` — the `luthier` binary

`args` is the clap surface, `main` dispatches and prompts, `render` turns
results into a table or JSON, `progress` draws the download bar. No decisions:
anything that looks like one belongs in `api`.

### `luthier-registry-tool` — the `luthier-registry` binary

`main` carries five subcommands — `validate`, `schema`, `hash-url`, `inspect`,
`check-updates` — and `upstream` asks a forge for the newest version. The last
three, and `validate --check-urls`, sit behind the `authoring` feature.

## Package lifecycle

```
luthier install surge
  │
  ├─ recover any journal left by an interrupted run
  ├─ load registry index          registry::build_index
  ├─ detect external packages     scan::detect_externals
  ├─ resolve                      resolver::resolve      → ordered plan
  ├─ refuse missing externals     ResolveError::ExternalMissing
  ├─ report unplayable content    engine::unplayable     (scan::detect_engines)
  │
  └─ for each package, dependencies first:
       ├─ download                download::Downloader   → cache/<sha256>
       ├─ verify SHA-256          (streamed; abort on mismatch)
       ├─ extract                 archive::extract       → transaction workspace
       ├─ plan destinations       install::plan          → derived, never declared
       ├─ validate                FormatInstaller::validate
       ├─ stage → rename          InstallTransaction::place  (journalled)
       └─ record                  state::StateGuard::commit
```

Nothing is installed before verification, and nothing is recorded before it is
installed.

## The seams

Four traits mark where the system is meant to grow. Each has more
implementations planned than exist today, which is why they are traits rather
than enums.

**`RegistryProvider`** — where manifests come from. `LocalRegistry` reads a
directory; `HttpSnapshotRegistry` fetches the bench as a release asset
(`bench.tar.gz`) over HTTPS and extracts it through the same hardened
extractor as any plugin;
`OasRegistry` reads an Open Audio Stack site, which publishes static JSON rather
than TOML manifests. Each is a `RegistrySource` variant in `config.rs`. Which
sources a user reads is fixed — `default_registries()` — so a new provider is
for this project's own use, not a way for users to add one. A `GitRegistry`
using `gix` would slot in
without touching anything that consumes an index. The MVP deliberately has no
git dependency: forges publish branch tarballs, and that is enough.

`OasRegistry` is the worked example of a backend whose source speaks a different
vocabulary. Every difference between the two is decided in one place,
`registry/oas/translate.rs`, so the rest of the system never learns that a
second schema exists — and what the source does not carry is marked rather than
invented. OAS says which *formats* an archive holds, not which *entry* is which,
so its artifacts carry `derive_install` and the rules are read from the verified
archive by `install::derive`, the same code behind `luthier-registry inspect`.
`validate` refuses the field in a hand-written manifest: rules written there are
rules someone reviewed. See [SECURITY.md](../SECURITY.md) for why that
distinction is the one that matters.

An artifact whose rules must be derived can promise no more than
`install::derive::DERIVABLE_FORMATS` covers, and `install::installable` checks
that *before* a download: a release declaring nothing on the list — a VST2
build, a standalone program — is refused by the resolver rather than fetched in
full and turned down by the installer afterwards.

Content is on that list but is read differently. A plugin announces itself with
an extension and a shape; a folder of samples announces nothing, so what is read
is the shape of the *archive* — one wrapper directory, or none — and it installs
as `<library root>/<package id>`. The ID rather than the wrapper's name because
the wrapper is `BillieDrum-48fadc01…`, renamed on every release; the ID is
already a validated path component and is what the user typed. Shape is reported
for every tree and acted on only where the artifact declares `library`, so a
plugin release that happens to hold no plugin is never installed as content.

**`ArtifactSource`** — expressed as the `type` field on a manifest's `source`.
`http` covers GitHub and GitLab release assets, which are ordinary URLs; `file`
serves the test suite. A future `github-release` variant naming owner, repo, tag
and asset can be added without a schema break.

**`FormatInstaller`** — one per plugin format, contributing only what genuinely
differs: the destination root, whether the format is a file or a bundle, and
what a valid one looks like. All staging, atomicity and rollback logic lives
once in `InstallTransaction`. `ClapInstaller`, `Vst3Installer` and
`Lv2Installer` are about forty lines each.

`LibraryInstaller` is the one that does not target a plugin root. Content —
sample libraries, preset packs, soundfonts — has no plugin format, so no format
root can supply its destination. That was the open design question, and the
answer was already in the trait: `FormatInstaller::root` is a hook, so the
library installer returns `Layout::library_root` and every other rule stays
intact. The manifest still never names a destination.

LV2 is the one whose validation is not an extension check: a bundle is a
directory carrying `manifest.ttl`, and its binaries may sit at any depth —
sfizz keeps them under `Contents/Binary/`. A binary is not required, because a
preset-only bundle is equally valid LV2, but any that is present is checked as
an ELF shared object for this architecture.

**`PackageSource`** — what the resolver reads. Implemented by `RegistryIndex`
in production and by in-memory fixtures in tests, so resolution is tested
without a registry on disk.

## Design decisions worth stating

### Manifests carry every release

The specification's example put a single `version:` at the top of each manifest.
Pinning (`luthier pin surge-xt 1.3.3`), update comparison and future lock files
all require resolving to a version that is not the newest, so a manifest instead
carries a `releases:` list. Static identity lives at the top level; anything
that changes between versions lives in the release.

### Artifacts are a list, not a map keyed by format

Upstream reality does not line up one archive per format. Surge XT ships a
single tarball containing both its CLAP and its VST3 — and also a much larger
one adding the standalone application. Dexed ships one zip with both. Dragonfly
Reverb ships four plugins in each of three formats. So a release carries a list
of artifacts, each declaring its target, the formats it provides, and what to
extract. Declaration order is the registry's preference; the client never
guesses.

### Destinations are derived

See [SECURITY.md](../SECURITY.md). A manifest names what to take out of an
archive, never where to put it.

### Two sources, in a fixed order

Two registries are read at once, and only two: Luthier's own bench
(`luthier-extras`, built from `bench/` in this repository) and the Open Audio
Stack registry. Users cannot add a source; `config.json` does not carry the
list, and `--registry-path` — hidden, for developing the bench — is the only
override. `Session::index()` merges both into one `RegistryIndex`, memoised
because building it three times in one `install` was a real regression rather
than a hypothetical one.

Where both carry the same package ID, the bench wins. That is not a tie-break:
it is the mechanism by which a curated manifest corrects a derived one, which
reading a large upstream registry makes necessary, and it is the bench's whole
reason to exist.

Neither source is signed: both are trusted on HTTPS and on the host that
serves them, which for the bench is this repository's releases.
`registry/provenance.rs` records what each fetch brought, when the source was
last asked, and the `ETag`/`Last-Modified` that make the next ask conditional;
it enforces nothing. See *Trusting GitHub* in [SECURITY.md](../SECURITY.md).

### There are no environments

0.2 had named environments: a `Layout` whose plugin roots, state and libraries
pointed under `~/.local/share/luthier/envs/<name>`, selected by `--env` or
`LUTHIER_ENV`. They were removed in 0.3. Hosts reach an environment's plugins
only through `CLAP_PATH`, `VST3_PATH` and `LV2_PATH`, the first two of which add
to the standard locations rather than replacing them, so CLAP and VST3 were
never isolated; the variables reach only a host started from that shell; and
each environment held its own copy of every sample library. What they were for
is served by `export`, `import --prune` and pins. See `docs/EXPORT.md`.

### Locations move roots, and never create them

`config.json` may name a directory for the cache, for sample libraries and for
plugins (`Locations`, applied by `Layout::with_locations`).

A chosen directory is typically on an external disk, so it is never created:
an unmounted disk leaves either nothing or an empty mount point, and
`create_dir_all` would then put the samples on the disk the user was trying to
spare. Every operation names the locations it touches and refuses when one is
missing (`Session::require_locations`). Removal needs this most — deleting
from an absent disk finds every file gone and would drop the package from
state with its files still on the disk.

State records absolute paths and removal deletes only under the current roots,
so `api::Storage` refuses to move libraries or plugins while anything is
installed there. Once the cache and the data directory can be on different
disks, a `rename` between them fails with `EXDEV`; `fsutil::move_tree` copies
instead where that happens. Staging for installs was already a sibling of the
destination, so the final rename never crosses a disk.

### State is authoritative, scanning is advisory

Ownership comes from the state file, never from scanning plugin directories. A
plugin whose name resembles a package is not that package. Scanning answers
different questions: whether an unmanaged file occupies a destination, whether
an `external` dependency is present, and whether an engine for a library's
content is.

### External packages

Some software has no redistributable Linux binary — sfizz, the reference SFZ
engine and the specification's own dependency example, has published none since
0.5.1 in 2020. Rather than invent metadata or pretend the dependency does not
exist, `kind: external` describes a package that can be depended on and detected
but never downloaded. This does not couple the manager to any distribution's
package manager: it only looks for files, and never invokes anything.

### Content and engines

A sample library is the one package that installs correctly and still does
nothing. Its manifest says what it holds (`content = ["drumgizmo"]`, read from
`contains` for the Open Audio Stack registry); the built-in engine list and
any bench's `engines.toml` say
which packages play that. After resolution and before any download,
`engine::unplayable` looks for one engine per content value — detected on disk,
recorded in state, or in the plan being executed — and reports the ones it
cannot account for. The plan carries the note and the confirmation decides;
refusing would treat an empty engine list as evidence about the machine, when
a registry with no field for what plays what (the Open Audio Stack's, for one)
produces an empty list for every library it carries.

It is not modelled as a dependency because a dependency names one package and
engines are interchangeable: a DrumGizmo kit plays in DrumGizmo or DrumCraker,
and depending on either would refuse the other's users.

The list starts in code — `luthier_manifest::builtin_engines` — and any bench
adds to it. That split took two goes to get right. Keeping it out of code
entirely was the first answer, on the grounds that engines appear faster than
this manager is released, and it put general knowledge in an optional place: a
user who configures only the Open Audio Stack registry, which has no field for
what plays what, was told nothing could play a library while sfizz sat
installed on their machine. Built-in entries carry detect rules where the
installed name is stable, which is what makes that user's copy count; a bench
still adds engines the day they appear, and registry entries come first so a
bench refines rather than collides. What a bench cannot do is remove one, which
is why the built-in list is narrow.

### Determinism

The same registry, target and request must always produce the same plan, or lock
files and reproducible exports are impossible later. Every point where
resolution could branch on iteration order branches on package ID instead:
worklists are `BTreeSet`, edges are sorted, releases are sorted by version
rather than trusted in file order, and search ties break on ID.

## Forward compatibility

The client parses manifests leniently: unrecognised fields are collected and
logged, never fatal, so an `luthier` built today keeps working against a registry
that has started emitting newer fields. The registry's own CI parses strictly,
so a contributor's typo fails the pull request. Both use the same types; only
the mode differs.

Unknown *values* are preserved too. Every string-valued enum carries an
`Other(String)` catch-all, so an unfamiliar plugin format round-trips verbatim
instead of corrupting the document, and validation rejects it where it matters
with a message naming the values this build knows.

## Not built

No GUI, no audio engine, no plugin host, no DAW integration. The manager
installs into the directories hosts already scan, so it is DAW-agnostic by
construction: REAPER, Ardour, Bitwig and Qtractor are all just consumers of
`~/.clap`, `~/.vst3` and `~/.lv2`, and nothing here knows about any of them.
That is a deliberate ceiling, not a gap — a DAW-specific integration would mean
writing into that DAW's own configuration, which is exactly the kind of
destination a manifest is forbidden from naming.

macOS and Windows are modelled but not built. `Os`, `Arch` and `Target` carry
all three, `Target::host()` recognises them, and the resolver selects on them —
but `Layout` builds Linux paths, the installers validate ELF64 shared objects,
and the registry holds no non-Linux artifacts. Adding a platform is a matter of
a `Layout` variant and per-format installers, not a schema change.
