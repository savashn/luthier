# The bench

The default bench lives in this repository, under `bench/`. The manager
consumes it as data; nothing is compiled in, and `bench/` is the one directory
here that is MIT rather than LGPL — see `bench/LICENSE`.

```
bench/
├── plugins/        lsp-plugins.toml, sfizz.toml, drumgizmo.toml
├── libraries/      sample libraries
├── presets/        preset packs
├── packs/          curated dependency sets
├── engines.toml    engines beyond the ones every build knows (optional)
└── LICENSE
```

Layout is for humans; the loader walks the whole tree. Two rules are enforced:
a manifest must be filed as `<id>.toml`, and an ID may appear only once.
`engines.toml` at the root is bench data rather than a package, and is skipped
by the walk. The JSON Schema is not here — it is generated from the manager's
types and lives at `schemas/package-v1.json` in the repository root, so there
is one copy rather than two to keep in step.

## Most packages do not belong here

**If the project publishes a Linux binary you can download, it belongs in the
[Open Audio Stack registry][oas] instead.** Luthier reads that too, an entry
there serves every OAS client rather than only this one, and a copy here would
be a second record of the same release to keep current — by hand, including
the checksum, on every upstream version. The bench also wins any ID collision,
so a stale copy here silently overrides a maintained entry there.

[oas]: https://github.com/open-audio-stack/open-audio-stack-registry

That is why this bench is six manifests against OAS's several hundred, and the
count is meant to fall rather than grow. What earns a place is what OAS cannot
express:

| | |
|---|---|
| `external` | Software packaged only by distributions, and only where something here needs it. `drumgizmo` and `sfizz` are engines that sample content plays in; the entry is what lets the manager say "install this from your distribution" rather than failing to resolve. If nothing needs it, it does not go here yet. |
| Sample content OAS does not carry | A DrumGizmo kit, until `contains` has a value for one. Declare what it holds with `content`, never which engine plays it. |
| `engines.toml` | An engine that appeared after a release. The built-in list already covers SFZ, SoundFont 2 and DrumGizmo content. |
| `packs` | Curated sets, which are a concept of this manager alone. |
| Shadows | A package OAS also carries, where its data or the derived install rules are wrong. Each says at the top of the file why it exists and what would retire it. |

When something here becomes expressible upstream, the move is the point:
send it to OAS and delete it from `bench/`. `surge-xt` and `dpf-plugins` both
left that way.

## How it is fetched

`luthier refresh` downloads `bench.tar.gz` from this repository's latest
release over HTTPS and extracts it through the same hardened extractor used
for plugin artifacts. The release workflow builds that asset from `bench/`.

It is an asset rather than a branch tarball of the repository for two reasons.
Discovery walks whatever it is handed, so a repository tarball would offer the
workspace's own `Cargo.toml` files up as manifests. And a signature is fetched
from the snapshot's URL with `.minisig` appended, which nothing can publish
under `/archive/refs/heads/` — a branch tarball is a snapshot that can never
be signed.

This bench and the Open Audio Stack registry are the only sources the manager
reads; users cannot add one. A package with a downloadable release belongs in
the Open Audio Stack registry, and this bench holds only what cannot be
expressed there.

There is deliberately no git dependency. A `GitRegistry` can be added later
behind the same `RegistryProvider` trait.

During development, skip fetching entirely:

```console
$ luthier --registry-path bench search synth
```

## Adding a package

```
fork → add manifest → run the validator → open a PR → CI → review → merge
```

First read *Most packages do not belong here* above. If the package has a
downloadable Linux binary, the pull request you want is against the Open Audio
Stack registry, not this one.

Then check the package is a candidate at all:

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
$ cargo run -p luthier-registry-tool -- validate bench
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

Validation itself runs as part of the test suite —
`the_real_bench_passes_strict_validation` drives the released binary against
`bench/` exactly as a reviewer would — so a bad manifest fails `ci.yml` like
any other change. `bench.yml` adds the one check no test may do, because the
suite is offline by rule: a sweep that fetches every artifact URL. It is
advisory, since reachability depends on hosts nobody here controls.

`ci.yml` also diffs `schemas/package-v1.json` against the one generated from
the manager's types, so the published schema cannot drift from what the code
actually accepts, and builds the validator with `--no-default-features` —
the configuration that keeps `luthier-manifest` free of async, HTTP and
archive decoders.

## Keeping the bench current

```console
$ luthier-registry check-updates bench
$ luthier-registry check-updates bench --all --json
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

Every release of the bench is signed, and the manager refuses one that is not.
The signature is [minisign](https://jedisct1.github.io/minisign/)'s format —
Ed25519 over the snapshot's BLAKE2b-512 — and the manager checks it between
the download and the extractor against the public key compiled into it
(`DEFAULT_BENCH_KEY` in `crates/luthier-core/src/config.rs`), so a snapshot
from a compromised forge account is refused rather than extracted.

```console
$ luthier-registry keygen --out luthier-bench.key
$ luthier-registry sign bench.tar.gz --key luthier-bench.key
```

`keygen` writes the same files `minisign -G -W` does: an unencrypted secret
key only its owner can read, and `luthier-bench.key.pub`. `sign` reads that
file, a stock minisign one, or 0.1's hex seed — which is how the current key
is stored — and writes two signatures beside the snapshot:

- `bench.tar.gz.minisig`, which the manager verifies and anyone can check
  with `minisign -Vm bench.tar.gz -P <public key>`;
- `bench.tar.gz.sig`, the format 0.1 reads. Publish it until no 0.1 client
  is left; then drop it from the upload and delete
  `registry::signature::legacy`.

Both are published at the snapshot's own URL with the suffix on the end. That
convention is where the manager looks, and it is not configurable.

### Releasing the default bench

The release workflow never sees the secret key. It builds the assets, attests
their build provenance, and creates the release as a **draft**, which
`releases/latest` does not serve. The rest happens on the maintainer's
machine:

```console
$ gh release download v0.2.0 -p bench.tar.gz
$ luthier-registry sign bench.tar.gz --key <secret key>
$ gh release upload v0.2.0 bench.tar.gz.minisig bench.tar.gz.sig
$ gh release edit v0.2.0 --draft=false
```

A published release is never re-run: replacing its `bench.tar.gz` would leave
the signature users verify pointing at bytes that are gone, which is a refusal
no flag relaxes. The workflow refuses to, and a fix goes out as a new tag.

Keep the secret key off the machine CI runs on, and backed up somewhere other
than the one it lives on. Losing it is a rotation that has to ship as a
release of the manager, since the key is compiled in.

### Rotating a key

Overlap, never a gap. A manager accepts only the keys compiled into it, so a
new key reaches users by release, and retiring the old one first would leave
every client that has not upgraded unable to refresh.

1. `luthier-registry keygen --out new.key`.
2. Release a manager whose default bench carries **both** keys (the `keys` of
   the first entry in `default_registries()`), still signing with the old one.
3. Once that release is widely installed, sign with the new key. Clients
   still on an older release refuse the bench and name the key ID they did
   not recognise, which is their cue to upgrade.
4. A later release drops the old key.

A key that has leaked is not a rotation, it is an incident: skip the overlap,
release a manager carrying only the new key, and say so plainly, because
every older client will keep trusting the leaked one until it upgrades.

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

## What is not in the bench, and why

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
