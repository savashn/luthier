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
- **`luthier-registry-tool`** is separate so the registry repository can depend on it
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
and prints the schema. Registry CI runs that build on every pull request, and
the manager's own CI builds it on every push so the configuration cannot rot in
the other repository.

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
| `engines` | `engines.toml`: which packages play which content |
| `discover` | `manifest_files`, the walk that finds manifests in a tree |
| `macros` | `string_enum!`, which gives every string enum its `Other(String)` arm |

### `luthier-core` — every decision

One library behind two binaries and, later, a GUI. `api` is the surface they
call; the rest is one pipeline, from a request to a recorded install.

| Module | Holds |
|---|---|
| `api` | `Session` — refresh, search, info, plan, install, remove, verify, update, cleanup, cache, benches, pins, export/import — and `Environments`, which manages environments from outside one |
| `registry` | Merging benches into one `RegistryIndex`, in configured order; `local`, `http` and `oas` providers; `provenance` for origin and key pinning; `signature` for Ed25519 over a snapshot |
| `resolver` | A request and an index into an ordered, deterministic plan |
| `download` | Fetching with a streamed SHA-256, resume, per-artifact ceilings, and the rules about when a `.part` survives |
| `archive` | Opening untrusted containers: `safe` is the single extraction policy, the per-format modules only say what entries exist |
| `install` | Journalled, atomic placement (`InstallTransaction`), per-format installers, and `derive`, which reads rules out of a verified tree |
| `state` | What is installed and which files belong to it — the authority on ownership |
| `scan` | What is present on the system but not installed by Luthier |
| `layout` | Every path, injected rather than computed from `$HOME` |
| `env`, `envfile` | Named environments, and the portable file `env export` writes |
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
  ├─ refuse unplayable content    engine::unplayable     (scan::detect_engines)
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
directory; `HttpSnapshotRegistry` fetches a tarball of the registry repository
over HTTPS and extracts it through the same hardened extractor as any plugin;
`OasRegistry` reads an Open Audio Stack site, which publishes static JSON rather
than TOML manifests. Each is a `RegistrySource` variant in `config.rs`, which is
what `luthier bench add` writes. A `GitRegistry` using `gix` would slot in
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

### Benches are ordered, and their origins are pinned

More than one registry is read at once — a *bench* is the kind, and
`luthier-pkgs` is the default one's name, as `homebrew-core` names the default
tap. `Session::index()` merges what each configured bench carries into one
`RegistryIndex`, memoised because building it three times in one `install` was
a real regression rather than a hypothetical one.

Where two benches carry the same package ID, configured order decides and the
first wins. That is not a tie-break: it is the mechanism by which a curated
manifest corrects a derived one, which reading a large upstream registry makes
necessary. `bench add` therefore appends rather than prepends — a bench added
without a word about precedence must not quietly start overriding the curated
one — and `--first` asks for the other behaviour explicitly.

Ordering decides which document is believed; `registry/provenance.rs` decides
whether it is the same document's author answering. Scheme, host and port are
recorded on first fetch, and a bench that later answers from a different origin
is refused before anything is downloaded from the new host. The digest is
recorded for audit rather than enforced, because a snapshot's contents change
on every refresh by design.

What turns that record into more than an audit trail is
`registry/signature.rs`. A bench may publish a detached Ed25519 signature
beside its snapshot, over the same SHA-256 the record carries, and
`HttpSnapshotRegistry::refresh` checks it between the download and the
extractor — so a snapshot nothing vouched for is never opened. The key is
pinned the same way the origin is: the first signature to arrive names the key
every later refresh must match, and a key written into `config.json` covers
the first fetch as well. `--allow-unsigned` accepts the *absence* of a
signature for one run and never discards the pin; a signature that fails to
verify, or one from a key this bench is not trusted to use, is refused
whatever any flag says. See [SECURITY.md](../SECURITY.md).

### Environments vary the layout, not the code

An environment is not a new subsystem: it is a [`Layout`] whose per-installation
parts — plugin roots, state, libraries — point under
`~/.local/share/luthier/envs/<name>`. Everything downstream is unchanged,
because nothing downstream ever built a path of its own. That is the payoff of
injecting `Layout` everywhere rather than reading `$HOME` where it is needed.

What deliberately stays shared is the artifact cache and the registry
snapshots. The cache is keyed by content hash, so two environments installing
the same plugin cost one download and two extractions. Libraries are per
environment, which can duplicate multi-gigabyte content; hardlinking a shared
store is the obvious later optimisation and needs no format change.

Selection is by `--env` or `LUTHIER_ENV`, never by a file recording a
"current" environment. A stored pointer would mean one terminal could change
what another terminal is about to install into.

The environment name becomes a path segment, so it is validated rather than
trusted: `--env ../../etc` is refused before any path is built from it.

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
`contains` for the Open Audio Stack registry); a bench's `engines.toml` says
which packages play that. After resolution and before any download,
`engine::unplayable` looks for one engine per content value — detected on disk,
recorded in state, or in the plan being executed — and the install is refused
when there is none.

It is not modelled as a dependency because a dependency names one package and
engines are interchangeable: a DrumGizmo kit plays in DrumGizmo or DrumCraker,
and depending on either would refuse the other's users. Nor is the engine list
in code: engines appear in registries far more often than the manager is
released.

### Determinism

The same registry, target and request must always produce the same plan, or lock
files and reproducible environments are impossible later. Every point where
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
