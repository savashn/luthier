# Changelog

Notable changes to the manager. A package added to or corrected in `bench/` is
not one of them — the git log says it better, and every manifest carries its
own version history in the `releases` it lists. What is recorded here is a
change to the *shape* of the bench: the schema, the layout, the conventions
`engines.toml` follows.

The format is [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project follows [semantic versioning](https://semver.org/). Until 1.0 the
command-line surface and the `--json` shapes may still change; the manifest
schema is versioned separately (`schema = 1`) and an older client is expected
to read a newer registry without installing from a category it cannot reason
about.

## [0.1.0] — 2026-09-27

The first release: everything below is new.

### Added

- **Commands.** `refresh`, `search`, `info`, `install`, `list`, `verify`,
  `update`, `remove`, `cleanup`, `pin`/`unpin`, `cache list`/`clean`,
  `bench list`/`add`/`remove`/`trust`/`untrust`, `location`, `env` (create, list,
  activate, deactivate, show, path, remove, export, import), `completions`
  and a man page the binary generates itself. Every command speaks `--json`,
  and every destructive one needs `--yes` when stdin is not a terminal —
  `--json` says how to render an answer, never that consent was given.
- **Formats.** CLAP, VST3, LV2 and sample libraries, installed from
  `.tar.gz`, `.tar.xz`, `.zip` and `.7z`, on Linux x86_64. Destinations are
  derived from the format, never named by a manifest.
- **Two registries read together.** The curated bench and the Open Audio
  Stack registry, merged in configured order so a local manifest can correct
  a derived one. Which packages play `sfz`, `sf2` and `drumgizmo` content is
  a list every build carries, which any bench's `engines.toml` adds to, so a
  library read from a registry with no field for that still knows what it
  needs — and an engine installed by a distribution still counts.
- **The default bench, in `bench/`.** Six manifests filed as `<id>.toml`:
  what the Open Audio Stack registry cannot express — two `external` engines,
  three DrumGizmo kits it has no `contains` value for, and one shadow that
  says at the top of the file why it exists and what would retire it. It is
  MIT rather than LGPL, and ships as the `bench.tar.gz` release asset, which
  is what a detached signature can be published beside.
- **Environments.** `--env` and `LUTHIER_ENV` redirect the per-installation
  parts of a layout; `env export` and `env import` reproduce an installation
  elsewhere, pinning every version including dependencies.
- **Locations on another disk.** `location set cache|libraries|plugins <dir>`
  moves downloads, sample libraries or plugins into a directory of the user's
  choosing — an external disk, typically — and `location reset` puts one
  back. The directory must already exist and is never created: while its
  disk is not mounted, anything that would write to it or delete from it is
  refused instead of filling the disk underneath. A location that packages
  are installed in cannot be moved away from until they are removed, and
  `location search-path` prints the exports hosts need to find plugins
  outside `~/.clap`, `~/.vst3` and `~/.lv2`.
- **Registry provenance.** A bench's origin — scheme, host and port — is
  pinned on first fetch, and one that later answers from somewhere else is
  refused before anything is downloaded from the new host.
- **Registry signatures.** A bench may publish a detached Ed25519 signature
  beside its snapshot; it is verified between the download and the extractor,
  and the signing key is pinned exactly as the origin is. `bench add --key`
  and `bench trust` require a key from the first fetch, `refresh
  --allow-unsigned` accepts a missing signature for one run without
  discarding the pin, and a signature that fails to verify is refused
  whatever any flag says. `luthier-registry keygen` and `sign` are the
  publishing side. The default bench ships with its public key built in, so
  even the first fetch is verified rather than pinning whatever signed it.
- **A bench that cannot be reached costs only itself.** `refresh` asks every
  configured bench, reports per bench, and keeps the snapshot a failed one
  already had; only a run where nothing could be refreshed is an error.
- **A bundle in the wrong shape is left alone, not taken apart.** A directory
  whose name claims a plugin format is listed and skipped rather than
  descended into: DPF's `ProM.clap` holds a binary next to the presets it
  loads, and deriving a rule for the binary alone installed a plugin stripped
  of its content.
- **Content that nothing plays is reported, not refused.** A library says
  what it holds; before downloading, the plan says what playing it takes and
  which engine is a command away. What a user does with a folder of samples
  is their business, and a registry with no field for what plays what — the
  Open Audio Stack's — would otherwise make every library it carries
  uninstallable.
- **Sample content from a registry that carries no install rules.** Where an
  artifact declares a `library`, the rule is read from the archive's shape —
  one wrapper directory, or none — and the content installs as
  `<library root>/<package id>`, a path that stays put across releases even
  though the directory inside the archive is named after a commit.
- **Refusal before the download.** A release is checked against what this
  build can actually install while the plan is being made: an artifact whose
  rules must be derived from its archive is refused unless it declares a
  format derivation can read. The Open Audio Stack registry carries
  standalone programs, VST2 builds and sample content, and each of those used
  to be downloaded in full — up to gigabytes — before the installer turned it
  down with a message about the archive.
- **A hardened extractor.** One extraction policy for every container, with a
  38-case malicious-archive corpus that asserts both the refusal and that
  nothing was written outside the extraction directory.
- **Registry tooling.** `luthier-registry validate` (the registry's CI gate,
  which builds without the network or an archive decoder), `hash-url`,
  `inspect`, `check-updates` against GitHub, and `schema`.

### Security

- No install scripts, and no field that could add one. A manifest cannot run a
  command or name a destination, which is what keeps a merged registry pull
  request from being code on a user's machine.
- Nothing is installed system-wide and root is never required. System plugin
  directories are read to detect distribution-provided packages and are
  excluded from the managed locations an uninstaller may delete from.
- Every artifact is verified against its manifest checksum before it is
  extracted, cache hits included.

[0.1.0]: https://github.com/savashn/luthier/releases/tag/v0.1.0
