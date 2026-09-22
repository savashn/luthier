# The registry

Packages live in a separate repository, [`luthier-pkgs`][registry]. The
manager consumes it as data; it has no built-in package list.

[registry]: https://github.com/savashn/luthier-pkgs

```
luthier-pkgs/
├── plugins/        lsp-plugins.toml, sfizz.toml, ...
├── libraries/      sample libraries
├── presets/        preset packs
├── packs/          curated dependency sets
├── schemas/        package-v1.json
├── engines.toml    engines beyond the ones every build knows (optional)
└── README.md
```

Layout is for humans; the loader walks the whole tree. Two rules are enforced:
a manifest must be filed as `<id>.toml`, and an ID may appear only once.
`engines.toml` at the root is registry data rather than a package, and is
skipped by the walk.

## How it is fetched

`luthier refresh` downloads a tarball of the repository's default branch over
HTTPS and extracts it through the same hardened extractor used for plugin
artifacts. There is deliberately no git dependency: forges publish branch
tarballs, and that is enough for the MVP. A `GitRegistry` can be added later
behind the same `RegistryProvider` trait.

During development, skip fetching entirely:

```console
$ luthier --registry-path ../luthier-pkgs search synth
```

## Adding a package

```
fork → add manifest → run the validator → open a PR → CI → review → merge
```

Before writing anything, check the package is a candidate at all:

- Is there an **official Linux x86_64 binary**? Many excellent projects publish
  source only. sfizz has shipped no Linux binary since 0.5.1 in 2020, which is
  why it is listed as `kind = "external"` rather than as an installable
  package.
- Is the archive **tar.gz, tar.xz, zip or 7z**? Those are what the extractor
  opens. Anything else needs a new format in `luthier-core` first.
- Is the **licence** clear, and does it match what you are about to write?
- Is the download URL **stable**? Point at a tagged release, never at "latest".
  Surge XT's `surge` repository publishes rolling nightlies under
  `/releases/latest`; its stable artifacts come from `releases-xt`.

### Write the manifest from the real artifact

Never hand-write a checksum or guess an archive's layout. The tooling reads
both from the actual file:

```console
$ cargo run -p luthier-registry-tool -- hash-url \
    https://github.com/asb2m10/dexed/releases/download/v1.0.1/Dexed-1.0.1-lnx.zip
        source: { type: http, url: "https://github.com/..." }
        archive: zip
        size: 7725160
        checksum: { sha256: "2bac3d0c4237e8c22c4274b6d5b59fe6329fcae72864f81b5830393eff354fc1" }
```

```console
$ curl -LO https://github.com/.../Dexed-1.0.1-lnx.zip
$ cargo run -p luthier-registry-tool -- inspect Dexed-1.0.1-lnx.zip
Format: zip
Entries: 9, 17787111 bytes

  Dexed
  Dexed.clap
  Dexed.vst3/
  LICENSE

Suggested install rules:

        install:
          - { format: clap, source: "Dexed.clap", kind: file }
          - { format: vst3, source: "Dexed.vst3", kind: bundle }
```

Transcribe both into the TOML shape shown in
[MANIFEST.md](MANIFEST.md). Archive layouts vary more than you would expect —
Dexed is flat, Fire nests under `CLAP/` and `VST3/`, Dragonfly Reverb nests
under a version-named directory — so read each one rather than assuming.

### Choose the category

Every package declares exactly one `category`, from a closed list:
`instrument`, `effect`, `utility`, `sample-library`, `preset-pack`, `pack`.
The validator rejects anything else. Everything finer grained goes in `tags`,
which is free-form. See [MANIFEST.md](MANIFEST.md#category-and-tags).

### Validate

```console
$ cargo run -p luthier-registry-tool -- validate ../luthier-pkgs
Checked 9 manifest(s): 0 error(s), 0 warning(s).
```

Add `--check-urls` to send a HEAD request to every artifact URL, and `--strict`
to treat warnings as errors. CI runs `--strict`.

### Test it for real

```console
$ luthier --root /tmp/luthier-test --registry-path . install <your-package>
$ luthier --root /tmp/luthier-test list
$ luthier --root /tmp/luthier-test remove <your-package>
```

`--root` keeps everything inside `/tmp/luthier-test`, so this cannot disturb
your own plugin directories.

## What CI checks

The validator runs in strict mode, so unrecognised fields are errors rather
than being ignored:

- schema version, package ID form, ID matches filename, no duplicate IDs
- semver versions, no duplicate versions within a package
- `category` is present and is one of the known values
- SPDX expression parses, is not deprecated, and supports the licence `kind`
- URL scheme matches the source type
- checksum present and well-formed
- target OS and architecture are recognised
- archive container can actually be extracted
- every install rule names an installable format with the right file/bundle shape
- no two rules install to the same destination
- no self-dependency, no duplicate dependencies
- `external` packages declare detect rules and no artifacts; packs declare
  dependencies and no artifacts
- `content` appears only on a `library`, uses known values, and every value is
  played by something — a built-in engine, or an entry in `engines.toml`
- `engines.toml` names each engine once, with known content, and detect rules
  only for formats that have a plugin directory

It also diffs the committed `schemas/package-v1.json` against the one generated
from the manager's types, so the published schema cannot drift from what the
code actually accepts.

## Keeping the registry current

```console
$ luthier-registry check-updates ../luthier-pkgs
$ luthier-registry check-updates ../luthier-pkgs --all --json
```

Asks each package's forge for its newest tag and reports what is behind. It
reads the repository from the **artifact URL** rather than the `repository`
field, because those disagree — Surge XT builds from `surge-synthesizer/surge`
and publishes from `releases-xt`.

It reports and stops there: a new version needs a checksum derived from the
real file, so `hash-url` remains a human step. `--exit-code` makes a stale
package fail the run; without it the command succeeds, which is what the
weekly CI job wants so it can file an issue instead of going red.

Set `GITHUB_TOKEN` to raise the API rate limit from 60 requests an hour to
5000. GitHub is asked for `releases/latest` and falls back to `tags` for a
project that only tags.

Only GitHub is implemented, and that is a decision rather than a gap. Every
manifest here that has an artifact points at GitHub; the three DrumGizmo kits
are served from a plain website with no API and are reported as such. A host
this command does not know is reported rather than guessed at, which is the
same rule that makes it report an unreadable tag instead of coercing one.

GitLab and SourceForge were built and taken back out. SourceForge has no tags
to ask for — `best_release.json` names a *file* — so a version has to be
inferred from a filename, and a project that publishes only source becomes an
`external` package here, which this command skips anyway. Self-hosted GitLab
cannot be recognised from a URL at all. Either is a small addition to
`upstream.rs` on the day a manifest needs one.

## Signing a snapshot

A bench may publish a detached Ed25519 signature beside its snapshot. The
manager checks it between the download and the extractor, and pins the key, so
a snapshot from a compromised forge account is refused rather than extracted.
Nothing here is required: an unsigned bench is read as it always was.

What is signed is the snapshot's SHA-256, which is also what the manager
records for audit.

```console
$ luthier-registry keygen --out luthier-bench.key
$ luthier-registry sign luthier-pkgs-2026.09.22.tar.gz --key luthier-bench.key
```

`keygen` writes a secret key only its owner can read and prints the public
key; `sign` writes `<snapshot>.sig`. Publish the signature at the snapshot's
own URL with `.sig` on the end — that convention is where the manager looks,
and it is not configurable, because a signature URL in a configuration file is
exactly the thing an attacker who could edit that file would point elsewhere.

**A signed bench publishes an uploaded snapshot, not a branch tarball.** A
forge generates `archive/refs/heads/main.tar.gz` on demand and there is
nowhere to put a signature beside it — so signing means cutting a release,
attaching the tarball, and pointing the bench's URL at that asset.

Users pin the key, or let the first signature pin itself:

```console
$ luthier bench add mine https://example.org/bench.tar.gz --key <public key>
$ luthier bench trust mine <public key>      # an existing bench
$ luthier bench list                         # shows what each bench is trusted to use
```

### Rotating a key

Overlap, never a gap. Retiring the old key first would leave a window in which
no refresh can succeed, and telling users to run `--allow-unsigned` through it
teaches exactly the wrong reflex.

1. `luthier-registry keygen --out new.key`, and publish the new public key
   where users already look for the old one.
2. Sign the next snapshots with the **old** key while users add the new one:
   `luthier bench trust <bench> <new public key>`. Both are accepted, so
   nothing breaks in either order.
3. Once the new key is widely trusted, sign with it instead. Users who have
   not added it see the key named in the refusal, which is what tells them a
   rotation happened.
4. Announce the retirement, and tell users to run
   `luthier bench untrust <bench> <old public key>`.

A key that has leaked is not a rotation, it is an incident: say so plainly,
publish the new key through whatever channel the old one did not compromise,
and expect users to check it against more than one source. A user who never
pinned a key is still protected against a *change* of signer — the pin from
their first fetch is what makes the change visible — but not against a
compromise that happened before they ever fetched.

## Sample libraries

Content — sample libraries, preset packs, soundfonts — uses `format = "library"`
in its install rules and `category = "sample-library"`. It is the one format
with no plugin root to derive a destination from, so it installs under the
library root instead:

```toml
kind = "library"
category = "sample-library"

[[releases]]
version = "1.1.0"

[[releases.artifacts]]
target = { os = "linux", arch = "x86_64" }
source = { type = "http", url = "..." }
archive = "tar.gz"
provides = ["library"]
install = [
  { format = "library", source = "VSCO-2-CE", kind = "bundle" },
]
```

The rules are the same as for a plugin: the manifest names what to take out of
the archive, never where it goes, and `rename` may change the leaf name and
nothing else. Either shape is accepted — a directory of samples or a single
soundfont file — because both are what upstreams actually ship.

A library that needs an engine declares what it holds with `content = ["sfz"]`
(or `sf2`, `drumgizmo`), never which engine plays it. Before downloading, the
manager looks for one of the engines it knows for that content — the built-in
list, plus anything a bench's `engines.toml` adds —
detected on the machine, installed, or arriving in the same command — and says
so in the plan when it finds none. It installs either way: a format implies
its player without any registry saying so, and refusing because *this*
registry has no entry would mistake what we know for what the machine can do. Adding an engine means one `[[engine]]` entry there: confirm it really
plays the content, and if its registry carries no detect rules, read the name
it installs as with `luthier-registry inspect <url>`. See
[MANIFEST.md](MANIFEST.md#content-and-engines).

## What is not in the registry, and why

The initial set is small on purpose. A working experience for a handful of
packages is worth more than a broken one for thousands.

Two of the nine manifests are `kind = "external"`: software that every
distribution builds and that publishes no redistributable Linux binary of its
own. The manager can depend on those and detect them, but will never pretend it
can install them. sfizz is the motivating case — no Linux binary since 0.5.1 in
2020 — and it is here because SFZ content needs an engine to be playable at
all. The rest of that group moved out when the Open Audio Stack registry became
the source of the plugins themselves.

The Aasimonster drum kit is absent deliberately: the other three DrumGizmo kits
state CC-BY-4.0 on their own wiki pages, and Aasimonster states nothing. A
manifest records what upstream states, so it is not listed.

If one of the external packages starts publishing a usable Linux archive, it
becomes a straightforward change from `external` to `plugin` plus a release.
