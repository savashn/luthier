# Changelog

Notable changes to the manager. A package added to or corrected in `extras/`
is not one of them — the git log says it better, and every manifest carries
its own version history in the `releases` it lists. What is recorded here is a
change to the *shape* of extras: the schema, the layout, the conventions
`engines.toml` follows. Up to 0.4, extras was called the bench.

The format is [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project follows [semantic versioning](https://semver.org/). Until 1.0 the
command-line surface and the `--json` shapes may still change; the manifest
schema is versioned separately (`schema = 1`) and an older client is expected
to read a newer registry without installing from a category it cannot reason
about.

## [Unreleased]

### Added

- **`luthier update --self`.** Luthier updates itself the way it was
  installed: a binary from the install script or the tarball is run once
  where it will live, then replaces the old one, with the man page and
  completions beside it; the .deb or .rpm goes through apt, dnf or zypper,
  with `sudo`, so the package manager keeps track of it, and root installs a
  copy it has checked again where the user cannot change it. A Luthier from
  Nix, or in a directory another package manager owns, is refused, with how
  to update it there. Every download is checked against the SHA-256 GitHub
  publishes for the release asset. It shows what it will do and asks first,
  as `install` does.
- **`update` and `refresh` say when a newer Luthier is out.** One request,
  given three seconds once the command has done its work; `--json`,
  `--quiet` and `--offline` skip it, and so does a Luthier from Nix, whose
  version is up to whatever installs it.
- **The Open Audio Stack list keeps itself current.** `search`, `info`,
  `install`, `update`, `remove` and `import` fetch it when it has never been
  fetched, and ask for a newer one when it was last asked more than a day
  ago, so nothing needs `refresh` to see new versions. A failure only warns,
  and the command goes on with the list it has; `--offline` skips it, and so
  does another Luthier refreshing the list at that moment. It never holds up
  an install in another terminal.

### Changed

- **Luthier's own manifests are built into it, and are called `extras`.**
  The handful of manifests Luthier corrects or adds to the Open Audio Stack
  registry with were fetched as `luthier-extras` from the latest release;
  they are now part of the binary, so they are there from the first command,
  offline too, and a binary only ever reads the ones it was released with.
  They change with Luthier itself (`luthier update --self`). `refresh` no
  longer lists them. Packages already installed from them keep
  `luthier-extras` as their recorded source, which is only informational.
- **`luthier bench list` is now `luthier sources`.** It lists the two
  sources, extras (`built-in`, located in `luthier <version>`) and the Open
  Audio Stack registry; its `--json` document is unchanged. There is no
  `bench` alias, so a script calling `luthier bench list` gets a usage error.
  "Bench" is no longer a word Luthier uses, and the manifests moved from
  `bench/` to `extras/` in this repository.
- **The Open Audio Stack list downloads only when it has changed.** Every
  request for it, `refresh`'s included, sends the `ETag` and `Last-Modified`
  the last one came with, and an unchanged list costs a 304 and no bytes
  (`refresh` says "unchanged"). It used to be downloaded whole, and twice, on
  every `refresh`: the first copy only to learn its SHA-256.
- **Two `refresh`es, or a `refresh` and an `install`, no longer meet.** Two
  refreshes at once could write the same snapshot together; one now waits
  for the other, under a lock of their own, apart from the one installs take.

### Removed

- **The `bench.tar.gz` release asset.** Nothing reads it any more, so
  releases no longer publish it: 0.2–0.4, which fetched it on `refresh`,
  will find it missing from the next release on and warn that
  `luthier-extras` was not refreshed, keeping the copy they have. Updating
  past 0.4 ends that.

### Fixed

- **A download no longer panics on its last attempt.** A resumed download
  that failed its checksum on the fourth try ended in a panic rather than
  the checksum error; it now reports the mismatch, and the next run starts
  the file over.

## [0.4.0] — 2026-10-06

### Added

- **Linux on 64-bit ARM (aarch64).** Every release now carries a static
  aarch64 build too, as a tarball, a .deb and an .rpm, built and tested on an
  ARM runner; `install.sh` picks the build for the machine it runs on, and
  the flake builds for `aarch64-linux`. On ARM, Luthier installs what is
  published for ARM: some 65 Open Audio Stack packages publish a Linux
  archive for aarch64, against some 330 for x86_64; the bench's own packages
  are x86_64 only. The end-to-end tests' fixtures publish for the
  architecture they run on rather than for x86_64.
- **A .deb, an .rpm and an install script** beside the tarball on every
  release, each carrying the same static binary, man page and completions.
  The packages put them where a distribution keeps them, so `man luthier`
  and completion work straight away and the package manager removes it all;
  `install.sh` installs the tarball into `~/.local` without root, and
  refuses it unless its SHA-256 is the one the release wrote into the
  script. Their names carry no version, so `releases/latest/download/`
  links stay correct, and the build provenance attestation covers them too.
  The README now sends each distribution to its own.

### Security

- **rustls 0.23.45** (RUSTSEC-2026-0285). 0.23.43 accepted TLS 1.3
  handshake messages sent at the wrong encryption level. The handshake
  transcript is still authenticated, so nothing could be altered or
  completed through it; every fetch goes through rustls, so it is updated
  anyway. `cargo deny check` now runs in CI, and weekly, to catch the next
  one.
- **sevenz-rust2 0.23.** 0.21.3 and 0.21.4 harden its 7z header parsing
  against malformed archives: a panic, an infinite loop, an unbounded
  allocation and an overflow in the coder stream count. A 7z is parsed after
  its checksum matches the manifest, so reaching this needed a merged
  manifest pointing at a crafted archive. 0.21.1's path-traversal fix is in
  the crate's own extraction helper, which Luthier never calls: every entry
  still goes through `archive/safe.rs`.

### Changed

- **Building from source needs Rust 1.93**, up from 1.89. sevenz-rust2 0.21
  and later require it. The release binary is unaffected.

## [0.3.0] — 2026-09-27

### Added

- **Nix: a flake and a Home Manager module.** `nix run github:savashn/luthier`
  runs it; `programs.luthier` declares the packages to install, where
  downloads, libraries and plugins go, and whether anything not declared is removed, and applies it on every
  `home-manager switch`. See `docs/NIX.md`.
- **`import --prune`.** Also removes every installed package the file
  neither names nor needs, so the installation ends up exactly as the file
  describes and importing it again does nothing. With `--prune`, a file
  naming no packages removes everything.
- **Plugins from Nix profiles are detected.** `/run/current-system/sw/lib`,
  `~/.nix-profile/lib`, `~/.local/state/nix/profile/lib` and
  `/etc/profiles/per-user/<user>/lib` are searched alongside `/usr/lib`, so
  an engine installed from nixpkgs counts for the sample libraries it plays.

### Changed

- **`luthier export` and `luthier import` replace `env export` and
  `env import`**, with the same file format and the same options. A file
  0.2 exported still imports.
- **An exported file may hold some packages at a version and not others.**
  A `version` is a hard requirement wherever one is given, not only in a
  `pinned` file, and `registry` may be left out.

### Fixed

- **Plugins a Fedora or openSUSE package installed are found.** Those
  distributions put 64-bit plugins in `/usr/lib64/lv2`, `/usr/lib64/vst3` and
  `/usr/lib64/clap`, which detection did not search, so an engine installed
  with `dnf` counted as absent. `/usr/lib64` and `/usr/local/lib64` are
  searched now.
- **No CA certificates no longer means a crash.** The HTTP client was built
  up front and panicked on a machine without a CA store, taking `file://`
  URLs and `--offline` down with it. It is built on the first request, and a
  missing store is an error that says what to install.

### Removed

- **Environments.** `luthier env create`, `list`, `activate`, `deactivate`,
  `show`, `path` and `remove`, the global `--env` flag and `LUTHIER_ENV` are
  gone, and so is `environments` in the Home Manager module. Hosts see
  CLAP and VST3 plugins in `~/.clap` and `~/.vst3` whatever `CLAP_PATH` and
  `VST3_PATH` say, so an environment never isolated them; the variables
  reached only a DAW started from that shell; and each environment kept its own
  copy of every sample library. `export`, `import --prune` and pins do what they
  were for. Directories 0.2 created under `~/.local/share/luthier/envs` are
  left in place and no longer read; delete them when you no longer need what
  is in them.
- **The `.sha256` files beside release assets.** GitHub shows every asset's
  SHA-256 on the release page, and `gh attestation verify` checks it along
  with where the file was built.

## [0.2.0] — 2026-09-27

### Added

- **Build provenance for every release.** The release workflow attests the
  binary and the bench through Sigstore, so a download can be traced to the
  workflow and commit that built it:
  `gh attestation verify luthier-x86_64-linux.tar.gz -R savashn/luthier`.

### Removed

- **Third-party benches.** `bench add`, `bench remove`, `bench trust` and
  `bench untrust` are gone. Luthier reads the Open Audio Stack registry and its
  own bench, which corrects it, and nothing else; `bench list` shows the two.
  A `registries` list in `config.json` is ignored, and no longer written.
- **Bench signatures.** The bench is no longer signed or verified; it is
  trusted on HTTPS and GitHub, as the binary is. `refresh --allow-unsigned`
  and `luthier-registry keygen` / `sign` are gone, and a release is published
  as soon as its tag is built. **0.1 requires the signature and refuses the
  bench from this release on** (the Open Audio Stack registry keeps working):
  upgrade to read it. `SECURITY.md` states what this trusts.
- **Origin pinning.** With the sources fixed in the binary, a pin protected
  nothing and would have locked every user out the day a release moved a URL.
  The provenance record remains, for audit.
- **`--registry-path` is hidden.** It still works, for developing the bench
  against a checkout.

### Fixed

- **A SoundFont library suggested a player nobody could install.** The only
  SoundFont engine known was `fluida-lv2`, which neither registry carries;
  `fluidsynth-clap`, which the Open Audio Stack registry does, is suggested
  too. It links the system's `libfluidsynth`.
- **A closed pipe no longer panics.** `luthier completions zsh | head`
  panicked outright, and `luthier --json search | head` on the first write
  after `head` exited. Luthier now ends quietly with status 141, as other
  command-line tools do.

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

[Unreleased]: https://github.com/savashn/luthier/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/savashn/luthier/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/savashn/luthier/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/savashn/luthier/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/savashn/luthier/releases/tag/v0.1.0
