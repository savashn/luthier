# Roadmap

Where Luthier is, what comes next, and why in that order.

Phases are ordered by what unblocks what, not by how interesting the work is.
Each item says what "done" means, so it can be closed rather than lingering.
Items in **Deliberate ceilings** are not backlog: they are things this project
has decided not to do, recorded so the decision does not have to be re-argued.

## Where it is today

Four crates, 386 tests, fully offline. `refresh`, `search`, `info`, `install`,
`list`, `verify`, `update`, `remove`, `cleanup`, `pin`/`unpin`, environments,
and `env export`/`env import` all work end to end against real packages.

CLAP, VST3, LV2 and sample libraries install, from `.tar.gz`, `.tar.xz`,
`.zip` and `.7z`, on Linux x86_64. Manifests are TOML; every package carries
one category from a closed list plus free-form tags. Extraction runs through
one hardened policy with a 38-case malicious-archive corpus.

Two registries are read together. The Open Audio Stack registry supplies
anything with a downloadable release binary — 458 of its 560 packages
translate, 293 of them installable on Linux x86_64 — and its install rules are
derived from the verified archive rather than declared. The curated bench is
down to 6 manifests: what OAS cannot express — two `external` engines, three
DrumGizmo kits it does not carry, and one shadow for an install rule nothing
can derive. What plays SFZ, SoundFont 2 and DrumGizmo content is no longer
its business: that list ships with the manager, and a bench's `engines.toml`
adds to it. A library whose engines are all absent
is reported before the download and installed anyway; the confirmation is where
the user decides. Where both registries carry an ID, the bench wins.

A bench's origin is pinned on first fetch, and a bench that publishes an
Ed25519 signature has it verified between the download and the extractor, with
the signing key pinned the same way. Nothing is signed yet, because nothing is
published yet.

What it is *not*: published. The manager repository is now a git repository;
the registry is not, neither has a remote, so neither CI workflow has ever
run and nobody but its author can install it. That is what Phase 0 is about.

---

## Phase 0 — Make it exist for other people

Nothing else matters until this is done. Every item below is small; together
they are the difference between a local working directory and a project.

### 0.1 Put both repositories under version control — half done

`luthier/` is a git repository and lives at `savashn/luthier`, privately.
`luthier-pkgs/` is neither: no repository, no remote. Two consequences
follow: `.github/workflows/ci.yml` and `.github/workflows/validate.yml` have
never executed, and the TOML migration and category system were verified only
on one machine.

The registry workflow checks out `savashn/luthier` and builds the validator
from source, so the manager repository has to exist first — and has to be
*readable* from the registry's CI. A workflow's default token reaches only its
own repository, so while the manager stays private that checkout needs a
personal access token in the registry's secrets. Making the manager public is
the cheaper answer, and the one this project's licence assumes.

**Done when:** both repositories are pushed, both workflows are green on a
pull request, and the schema-drift check in each has run at least once.

### 0.2 Add the licence texts — done

`luthier/LICENSE` carries the canonical LGPL-2.1 text, which is a standalone
licence and needs no GPL text beside it. `luthier-pkgs/LICENSE` carries MIT plus
a scope note covering the generated schema and the fact that manifests record
upstream licences rather than granting them.

The split is deliberate: the tool stays free (LGPL-2.1-or-later, matching both
the stated GNU philosophy and the overwhelmingly copyleft Linux audio
ecosystem), the data stays maximally reusable (MIT, matching homebrew-core,
nixpkgs and winget-pkgs).

Lesser rather than plain GPL because of what this codebase is for. `luthier-cli`
holds no decisions precisely so that something else can call
`luthier_core::api` — a GUI first, and a DAW's own integration after that. Under
the GPL that caller would have to be GPL too; under the LGPL it can carry its
own licence while every change to Luthier itself stays copyleft, which is the
line this project actually cares about. Invoking the binary and reading
`--json` was never restricted under either, and remains the integration path
that asks nothing of the caller.

**The "or later" is not boilerplate.** Five crates in the runtime tree are
Apache-2.0 only — `spdx` in `luthier-manifest`, `sevenz-rust2` and its
`lzma-rust2` in `luthier-core`, `sync_wrapper` under reqwest, `zopfli` under
zip — and the FSF reads Apache-2.0 as incompatible with the version-2 licences
because of its patent-termination and indemnification clauses, and compatible
with version 3. Declaring `LGPL-2.1-or-later` rather than `-only` is what
keeps a build distributable: a recipient may take the whole thing under
LGPL-3.0-or-later, where the conflict does not arise. Dropping the "or later"
would make the dependency tree unshippable, so it is a decision rather than a
default.

### 0.3 Publish a release, and make the default registry resolve

`config.rs` points at `https://github.com/savashn/luthier-pkgs/archive/refs/heads/main.tar.gz`.
Until that URL resolves, `luthier refresh` fails out of the box and every user
must pass `--registry-path`. The comment in the source says so; it stops being
true the moment 0.1 lands.

The only install path today is `cargo build --release` with a Rust 1.89
toolchain. The people this tool is for are musicians.

The machinery for this exists: `.github/workflows/release.yml` builds a
statically linked musl binary on a `v*` tag, asserts it really is static
rather than trusting the target triple, packages it with the man page and
completions the binary generates itself, and publishes it with its SHA-256
under a versionless asset name so the README can name a `releases/latest`
URL that stays correct. Release notes come from `CHANGELOG.md`, and the job
refuses a tag that disagrees with the workspace version. The README's install
section leads with that download.

What is left is the part only an account can do: tag, let it run, and publish
the registry so `config.rs`'s default URL resolves.

**Done when:** ~~the README's install section leads with the binary rather
than with cargo~~, a tagged `v0.1.0` publishes a static `luthier` binary for
linux-x86_64, and `luthier refresh` works on a clean machine with no flags.

### 0.4 Repository hygiene — done

`CHANGELOG.md`, `CODE_OF_CONDUCT.md`, issue and pull-request templates in
both repositories, and a `.gitignore` for the registry that keeps the
archives `hash-url` and `inspect` pull down out of a repository whose whole
point is that it mirrors nothing.

The templates are not generic. The manager's feature-request form names the
deliberate ceilings and where their reasoning lives, so a request to lift one
starts by answering it; its pull-request template lists the four CI gates and
says which one surprises people and why. The registry's asks first whether a
package belongs in the Open Audio Stack registry instead, and its
pull-request template asks for the checksum and the install rules to have
been *derived* rather than written.

The two changelogs cover different things on purpose: the manager's is
releases of a program, the registry's is changes to the shape of a
collection. A package being added is not a change to either, which is what
the git log is for.

**Done when:** ~~present in both repositories, with the changelog covering
`v0.1.0`~~.

---

## Phase 1 — Make the registry maintainable

A curated registry dies of neglect, not of bad design. That is why the bench
carries only what OAS cannot: six manifests, of which three have an artifact
pinned to a version upstream will move past. The rest — two `external` engines
and one shadow — have nothing of their own to go stale.

The `external` entries were cut from twenty to two on the same reasoning. An
entry nothing depends on is an unverified claim — a bundle name and an
`apt install` line — that no tooling watches, in a registry whose whole promise
is checked data.

### 1.1 `luthier-registry check-updates` — done

Every comparable project solved this early: Scoop has `checkver`/`autoupdate`,
Homebrew has `brew livecheck` plus an autobump bot, nixpkgs has
`nixpkgs-update`, winget has `wingetcreate`. Without it, version tracking is a
person reading release pages, and the registry cannot grow past roughly its
current size.

`check-updates` walks the registry, asks each package's forge for its newest
tag and reports what is behind. Weekly in registry CI
(`.github/workflows/check-updates.yml`), filing a rolling issue rather than
failing the build — a package being behind is news, not a broken registry.

Three decisions worth keeping:

- The upstream repository comes from the **artifact URL**, not the `repository`
  field. Those disagree: Surge XT builds from `surge-synthesizer/surge` and
  publishes from `releases-xt`, so asking the source would report wrong
  versions forever.
- Nothing is rewritten. A new version needs a checksum from the real file
  (§26), so the command reports and a human runs `hash-url`.
- A tag that is not a version is reported as unreadable rather than coerced,
  and a rate limit is named rather than passing as "up to date" — the two ways
  this command could be confidently wrong.

**GitHub is the whole list, and that is now a decision rather than a gap.**
This once read "GitLab and SourceForge would each be a small addition to
`upstream.rs`". Both were written, and both came back out.

The data is the first half of the reason: of the bench's nine manifests, the
six with an artifact point at GitHub three times and at drumgizmo.org three
times. Nothing uses either forge, and this command never sees the Open Audio
Stack registry at all — it walks a directory of manifests.

The second half is that the two are not equally cheap. SourceForge has no tags
to ask for: `best_release.json` names a *file*, so a version has to be inferred
from a path like `/qtractor/1.6.4/qtractor-1.6.4.tar.gz` — inference, in the
one command whose whole discipline is not guessing. And the projects that would
benefit mostly publish source only, which makes them `external` packages here,
which this command skips by design. Self-hosted GitLab cannot be recognised
from a URL at all, so `gitlab.example.org` would have stayed unsupported
anyway; only `gitlab.com` was ever covered.

So a host this command does not know is reported as having no API, the three
DrumGizmo kits included, and a forge gets an arm on the day a manifest points
at it.

### 1.2 Test the rest of `luthier-registry-tool` — done

The tool went from zero tests to 33: ten unit tests, ten for `check-updates`
and thirteen in `tests/validate.rs`, which drives the binary the way registry
CI does. `validate` — the component registry CI depends on to catch a
contributor's mistake — now has a case per rule class: filing, duplicate IDs,
a parse error costing one file rather than the run, strict mode, an accepted
warning, content with no engine, a broken `engines.toml`, and a pass over the
real bench when it is checked out.

`inspect`'s rule suggestion has cases too. They were added alongside the fix
for its LV2 arm, which listed a bundle and suggested nothing, so every manifest
written from `inspect` silently lost the format most Linux plugins ship. The
rendering is tested in the tool and the walking in `luthier-core::install::derive`,
which is where the rules live.

`hash-url` has cases now, against a release served by `wiremock` on localhost:
the four fields it prints in the shape they are pasted into a manifest, the
container inferred from the URL for each archive format the registry uses, a
URL that names no container printing no `archive` line at all, and — the one
that matters — a failed download printing no digest. An error page hashes to
something, and a digest printed for one would end up in a manifest and verify
forever against 404 bytes.

`inspect` now covers all three layouts a release ships in: version-nested,
flat, and one directory per format. The rule rendering moved into a function
of its own so the assertion is against the exact line a contributor pastes,
and two more cases pin down what must *not* produce a rule — a `.clap` inside
a `.vst3` bundle, and DPF's `ProM.clap`, which is a directory.

**Done when:** ~~`hash-url` has cases, and `inspect` covers the flat and
format-nested layouts as well as the version-nested one it already has~~.

### 1.3 Split the tool so registry CI stops building the world — done

`luthier-registry-tool` now has an `authoring` feature, on by default, carrying
everything that needs the network or an archive decoder: `hash-url`, `inspect`,
`check-updates` and `validate --check-urls`. Without it the crate builds
`validate` and `schema` from `luthier-manifest` alone.

| | crates | clean build | binary |
|---|---|---|---|
| default | 164 | 27 s | 11.8 MB |
| `--no-default-features` | 70 | 6 s | 2.8 MB |

No tokio, no reqwest, no zip/tar/7z/xz, no `luthier-core`. Registry CI's
per-pull-request `validate` job uses it; the `urls` job and the weekly
`check-updates` job still need the full build, which is correct — both make
network requests.

Two changes made this possible, both worth knowing:

- **`manifest_files` moved from `luthier-core` to `luthier-manifest`.**
  Validating a tree should not need a runtime to list files. `luthier-core`
  re-exports it, so there is still one implementation of the rules — notably
  that a symlink in an untrusted snapshot is skipped. This narrows the crate
  map's ban on "filesystem policy" in `luthier-manifest` to mean *installation*
  policy: `Layout`, destinations, ownership. Discovery is not that.
- **`cmd_validate` became synchronous.** It was `async` only because
  `--check-urls` lived inside it; the URL sweep is now a separate,
  feature-gated step. The validation pass is byte-identical in both builds.

The manager's CI builds `--no-default-features` on every push, so the
configuration cannot rot silently in the other repository.

---

## Phase 2 — Close the trust gaps — done

Neither item below was a live vulnerability. Both were places where the
security model rested on something narrower than it should.

### 2.1 Verify the registry snapshot — done

Package artifacts are checksummed against the manifest. The *registry itself* —
the document carrying those checksums — is fetched by `download_unverified` in
`registry/http.rs` and trusted on HTTPS alone. The chain is only as strong as
its first link.

`registry/provenance.rs` closes the half of this that trust-on-first-use can
close. What it pins is the thing that should never change rather than the thing
that always does: a snapshot's *contents* change on every refresh — that is
what a refresh is for — so pinning its digest would reject every genuine
update. Its *origin* should not change at all. So scheme, host and port are
recorded on first fetch and `check_origin` refuses a bench that later answers
from somewhere else, before anything is downloaded from the new host. A path
that moves within one origin is upstream reorganising itself, which is theirs
to do. `bench remove` forgets the record, so re-adding a name under a different
URL is not refused for a pin the user has already discarded.

The digest and byte count are recorded rather than enforced, which makes a
change auditable and gives a signature something to be checked against later.
Enforcing them is 2.2's job, not this one's: without a signature there is
nothing to say which digest is the right one.

**Done when:** ~~the recorded digest is checked against a signature over the
snapshot — which is to say, when 2.2 lands~~. It is, and it does: the digest
`provenance.rs` records is exactly what a signature covers, so the two halves
are statements about the same bytes rather than two separate records. Both are
in `SECURITY.md`, under *Registry provenance* and *Registry signatures*.

### 2.2 Ed25519 signature verification — done

A bench may publish a detached signature beside its snapshot, at the
snapshot's own URL with `.sig` on the end. `registry/signature.rs` verifies it
between the download and the extractor, so a snapshot nothing vouched for is
never opened and a refusal always leaves the previous one in place.

Four decisions worth keeping:

- **What is signed is the digest, not the bytes.** It is the digest
  `provenance.rs` already records, so verification needs no second pass over a
  tarball and 2.1's audit trail becomes the thing a key vouches for. A context
  string is prefixed before signing, so a signature over a snapshot can never
  be replayed as a signature over anything else this project signs later.
- **The signature file carries the public key.** That is what makes a first
  fetch worth anything: there is nothing yet to check against, so the key it
  names is pinned exactly as the origin is, and every refresh after it has
  something to match. A key configured with `bench add --key` or `bench trust`
  covers the first fetch too, which is the fetch an attacker would aim at.
- **The override covers absence and nothing else.** `refresh
  --allow-unsigned` accepts a bench that was signed before and is not now, for
  one run, and never discards the pin — otherwise using it once would turn
  verification off for good. A signature that fails to verify, or one from a
  key the bench is not trusted to use, is refused whatever any flag says:
  those are claims that did not hold up rather than absent ones. So is an
  unreadable signature file, or publishing garbage would be a way to turn
  verification off.
- **A signed bench publishes an uploaded snapshot.** A forge generates a
  branch tarball on demand and there is nowhere to put a signature beside it.
  That is a change to how a bench is published, not to how it is read, and it
  is why nothing here forces a bench to sign.

`luthier-registry keygen` and `sign` are the publishing side. Key
distribution remains what it always was — publish the public key where users
already look — and the rotation procedure is in `docs/REGISTRY.md`: trust the
new key before retiring the old one, because the other order leaves a window
in which no refresh can succeed and teaches users to reach for
`--allow-unsigned`.

**Done when:** ~~the manager verifies a detached signature over the registry
snapshot, refuses an unsigned or badly-signed one unless explicitly
overridden, and the key rotation procedure is written down~~.

---

## Phase 3 — Reach

### 3.1 Distribution packaging

An AUR package, a `.deb`, and ideally a Flatpak or a static binary in a release
asset. `cargo install` is not a distribution channel for this audience.

**Done when:** at least the AUR package exists and is referenced from the
README.

### 3.2 `luthier bench add` — done

`RegistryProvider` and `RegistrySource` already supported multiple registries;
what was missing was a way to add one without hand-editing `config.json`. This
is what makes third-party collections possible at all — Homebrew's taps,
Scoop's buckets.

`bench list`, `bench add <name> <url|path>` and `bench remove <name>` now exist.
`add` takes a directory, a snapshot tarball URL or an Open Audio Stack site
root, inferring which from the location and accepting `--type` to override. It
appends rather than prepends, because priority decides which manifest wins a
collision and a bench added without a word about precedence must not quietly
start overriding the curated one; `--first` asks for that explicitly. All three
read and write the persisted configuration rather than a `--registry-path`
override, so they describe the same thing.

Removing a bench deletes its snapshot and forgets its pinned origin, so
re-adding the name under a different URL is not refused for a pin the user has
already discarded.

A collection of manifests is a *bench*; `luthier-pkgs` is the default one
(see [Open questions](#third-party-registries)). The naming convention there —
whether `bench add savas/jazz` should expand to a repository URL — is still
open; today the location is written out in full.

**Done when:** ~~`luthier bench add <name>`, `bench list` and `bench remove`
exist~~, and a package from a third-party bench can be searched, installed and
removed.

### 3.3 A `GitRegistry` backend

Branch tarballs were the right call for the MVP: no git dependency, and forges
publish them. A `gix`-based backend would make incremental refresh cheap.

It would need its own answer on signatures rather than inheriting 2.2's. A
snapshot is one file, so a detached signature can sit beside it; a repository
is a history, and what gets signed there is a tag or a commit. The policy —
which key, pinned how, and what a refusal leaves in place — should be the
same, which is why it lives in `registry/signature.rs` rather than inside the
snapshot provider.

**Done when:** `RegistrySource::Git` exists and `refresh` updates without
re-downloading the whole tree.

### 3.4 Grow the registry

Concrete content gaps, in order of how visible they are:

- **`presets/` is empty.** The `preset-pack` category and kind both exist with
  no manifest using them. Reading the Open Audio Stack registry's own
  `presets/` index is the other half of this, and it now has an answer to
  follow: content derives its rule from the archive's shape and installs under
  the package ID, exactly as a `library` does.
- **No impulse-response content.** The vision document promises IRs; the
  registry has plugins that *load* IRs and no IR collections.
- **No aarch64 artifacts.** The schema models the target; an ARM Linux user can
  resolve nothing.
- **Only two packages are `external`, and both are engines.** That is the point
  now: the bench carries what OAS cannot, and `check-updates` is what will
  notice when an upstream starts publishing binaries.

**Done when:** at least one preset pack and one IR collection are listed, and
the README's claims match what the registry actually carries.

---

## Phase 4 — Other platforms

macOS and Windows are modelled throughout — `Os`, `Arch`, `Target::host()`, and
the resolver all handle three platforms — but `Layout` builds Linux paths, the
installers validate ELF64 shared objects, and the registry holds no non-Linux
artifacts.

This is deliberately last. Adding a platform is a `Layout` variant plus
per-format installers plus registry content; it is not a schema change, and
nothing done before this point makes it harder. Linux first is a scope
decision, not an architectural one.

**Done when:** `luthier install` works on macOS with the same manifests, and
`env import` reproduces a set across the two — the point at which the vision
document's "even to a different operating system" becomes true.

---

## Deliberate ceilings

Not backlog. These are decided.

**No install scripts, ever.** No field exists to run a command and none will be
added. A merged registry pull request must not be able to execute anything on a
user's machine. This is why a package that cannot be installed by copying files
into a format's directory is listed as `external` instead.

**Manifests never name a destination.** An install rule gives a format, a path
inside the archive, and a kind; the destination is derived. A `destination`
field would be an arbitrary-write primitive for anyone with a merged pull
request.

**No DAW integration.** The manager installs into the directories hosts already
scan. REAPER, Ardour, Bitwig and Qtractor are all consumers of `~/.clap`,
`~/.vst3` and `~/.lv2`, and nothing here knows about any of them. A DAW-specific
integration would mean writing into that DAW's own configuration — exactly the
kind of destination a manifest is forbidden from naming.

**Not a DAW.** No audio engine, no plugin host, no MIDI. A GUI is possible
later and would call `luthier_core::api::Session` unchanged, which is why
`luthier-cli` holds no decisions.

**No system-wide installation.** Everything is user-local; root is never
required and system directories are never written to. They are read to detect
`external` packages and are deliberately excluded from
`Layout::is_managed_location`, so no state file can direct a delete into them.

---

## Open questions

Things without an answer yet, recorded so they are not mistaken for oversights.

**How does an environment file handle a package that has left the registry?**
Import currently fails, which is right for reproducibility and unhelpful when a
project simply moved. A `--skip-missing` flag is the obvious answer; whether it
should also record what it skipped is not settled.

**Should libraries be shared between environments?** They are per-environment
today, which duplicates multi-gigabyte content. Hardlinking from a shared
content-addressed store needs no format change and is the obvious optimisation,
but it interacts with `verify` in ways that need thinking through.

**What is the update story for a `pack`?** A pack resolves to dependencies, so
updating one means updating its members — but a pack's own version bump and its
members' bumps are different events, and `update` does not currently
distinguish them.

### Third-party registries

Four decisions sit behind 3.2, and they are entangled: the repository naming
convention is the visible end of a design question that has not been answered.
Recorded together because deciding one in isolation will paint the others into
a corner.

**What happens when two registries carry the same package ID? — decided:
precedence.** Configured order wins, and the curated bench is configured first.
The reason is not abstract: reading the Open Audio Stack registry means
correcting it in places, and the only way to correct a package is for a local
manifest to beat the remote one. `refuse` would kill that outright; namespacing
would make `PackageId` two-part for the sake of a collision that, across 559
upstream packages and 28 local ones, happens three times. Each shadow says at
the top of the file why it exists and what would retire it.

The shapes considered:

- *Namespacing* — `luthier install jazz/surge-xt`, ambiguity is a hard error.
  Homebrew does this (`user/tap/formula`). Honest, but it makes `PackageId` a
  two-part thing, and `PackageId` is currently a validated single segment used
  as a path component — a security boundary, not just a string.
- *Precedence* — configured order wins, first match resolves. Cheapest to
  build, and the one that will silently install the wrong package one day.
- *Refuse* — a collision is an error and the user disambiguates. Safest, and
  annoying exactly when someone forks the default bench to change one
  manifest, which is the common case.

None is obviously right. What is clear is that state records `registry` per
installed package already, so whatever is chosen, `verify` and `update` can
tell where something came from.

**Do third-party repositories carry a required name prefix?** Homebrew requires
`homebrew-<name>` and adds the prefix itself, so `brew tap savas/jazz` fetches
`github.com/savas/homebrew-jazz`. The user types something short, repositories
are named consistently, and searching a forge for `homebrew-*` discovers every
tap in existence — discoverability for free. Scoop takes the opposite line: a
full URL, no convention, no discovery. A `luthier-bench-<name>` convention
would buy the same thing here.

**What does the user type?** A short handle resolved against a forge, a full
URL, or both. Tied to the previous question: a prefix convention only pays off
if the short form exists.

**What is this thing called? — decided: a `bench`.** Homebrew coined "tap",
Scoop coined "bucket"; ours is the workbench a luthier keeps their materials on.

A bench is the **kind of thing**, not a second kind. Every collection of
manifests is a bench, including the official one — exactly as `homebrew-core`
*is* a tap and `ScoopInstaller/Main` *is* a bucket. Neither ecosystem invented
a separate word for its own collection, and neither should this one.

What stays is the distinction between the kind and an instance's name:

| Term | Names |
|---|---|
| *bench* | The kind. A collection of manifests. |
| `luthier-pkgs` | The default bench, by name — as `homebrew-core` names the default tap rather than being called `homebrew-tap`. |
| "registry" in code | The mechanism: `RegistryProvider`, `RegistryIndex`, `RegistrySource`. An implementation term, said 178 times, unchanged. |

So the official repository is **not renamed**. An instance's name need not be
the kind word, and in both precedents deliberately is not: `luthier-pkgs`
says what that particular collection is — curated, reviewed, the default —
where `luthier-bench` would only repeat the type and require knowing the
metaphor to parse.

Third-party benches are what the command exists for:

```console
$ luthier bench add savas/jazz     # fetches github.com/savas/luthier-bench-jazz
$ luthier bench list
$ luthier bench remove jazz
```

That implies the prefix convention above: third-party repositories are named
`luthier-bench-<name>` and the client adds the prefix, so users type something
short and a forge search for `luthier-bench-*` discovers every bench in
existence. The default bench predates the convention and keeps its own name,
which is also how `homebrew-core` sits alongside `homebrew-<tap>`.

That difference in provenance is what the conflict question turns on: the
default bench is curated and reviewed, a third-party one is whatever its author
put there, so precedence between them is not a symmetric choice.

Still open: whether `bench add` also accepts a bare URL for benches that do not
follow the convention (Scoop allows this; Homebrew does not).
