# The registry

Packages live in a separate repository, [`luthier-pkgs`][registry]. The
manager consumes it as data; it has no built-in package list.

[registry]: https://github.com/luthier/luthier-pkgs

```
luthier-pkgs/
├── plugins/        lsp-plugins.toml, sfizz.toml, ...
├── libraries/      sample libraries
├── presets/        preset packs
├── packs/          curated dependency sets
├── schemas/        package-v1.json
├── engines.toml    which packages play which content
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
  played by some engine in `engines.toml`
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
5000. Only GitHub is implemented; other hosts are reported as unsupported
rather than guessed at.

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
(or `sf2`, `drumgizmo`), never which engine plays it. The manager refuses to
download it unless one of the engines `engines.toml` lists for that content is
present. Adding an engine means one `[[engine]]` entry there: confirm it really
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
