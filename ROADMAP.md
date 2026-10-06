# Roadmap

Where Luthier is, what comes next, and why in that order.

Phases are ordered by what unblocks what, not by how interesting the work is.
Each item says what "done" means, so it can be closed rather than lingering.
Items in **Deliberate ceilings** are not backlog: they are things this project
has decided not to do, recorded so the decision does not have to be re-argued.

## Where it is today

Four crates, 404 tests, fully offline. `refresh`, `search`, `info`, `install`,
`list`, `verify`, `update`, `remove`, `cleanup`, `pin`/`unpin`, and
`export`/`import` all work end to end against real packages. Named
environments existed until 0.2 and were removed; see *Deliberate ceilings*.

CLAP, VST3, LV2 and sample libraries install, from `.tar.gz`, `.tar.xz`,
`.zip` and `.7z`, on Linux x86_64. Manifests are TOML; every package carries
one category from a closed list plus free-form tags. Extraction runs through
one hardened policy with a 38-case malicious-archive corpus.

Two registries are read together. The Open Audio Stack registry supplies
anything with a downloadable release binary — 464 of its 560 packages
translate, 299 of them installable on Linux x86_64 — and its install rules are
derived from the verified archive rather than declared. The curated bench is
down to 7 manifests: what OAS cannot express — two `external` engines, three
DrumGizmo kits it does not carry, one shadow for an install rule nothing can
derive, and one for an archive that holds two builds of the same plugin. What plays SFZ, SoundFont 2 and DrumGizmo content is no longer
its business: that list ships with the manager, and a bench's `engines.toml`
adds to it. A library whose engines are all absent
is reported before the download and installed anyway; the confirmation is where
the user decides. Where both registries carry an ID, the bench wins.

Those two sources are the only ones, and users cannot add a third: the
bench exists to correct OAS, not to host collections of its own. Neither is
signed: both are trusted on HTTPS and on GitHub, as the binary is. The release
workflow attests the build provenance of everything it publishes.

`v0.3.0` is the latest release: a static binary for Linux x86_64 and the
bench, from the public repository at `savashn/luthier`, published straight
from a `v*` tag. Phase 0 is done.

---

## Phase 0 — Make it exist for other people

Nothing else matters until this is done. Every item below is small; together
they are the difference between a local working directory and a project.

### 0.1 Publish the repository — done

`luthier/` is public at `savashn/luthier`. `ci.yml` has run on every push to
`main` since 2026-09-18, the `--no-default-features` build of the validator
and the schema-drift check included. The release's musl build is 0.3; it
first ran for `v0.1.0`.

There used to be a second repository to publish. The bench now lives here
under `bench/`, which removed the part of this item that was genuinely awkward:
the bench's CI had to check out `savashn/luthier` to build the validator, and
a workflow's default token reaches only its own repository, so a private
manager meant a personal access token in the other repository's secrets. One
repository needs none of that. Making the manager public remains the right
answer anyway, and the one this project's licence assumes.

**Done when:** ~~the repository is pushed and public, and `ci.yml` is green
with the schema-drift check having run at least once~~.

### 0.2 Add the licence texts — done

`luthier/LICENSE` carries the canonical LGPL-2.1 text, which is a standalone
licence and needs no GPL text beside it. `bench/LICENSE` carries MIT plus
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

### 0.3 Publish a release, and make the default registry resolve — done

`config.rs` points at `https://github.com/savashn/luthier/releases/latest/download/bench.tar.gz`.
Until a release existed that URL did not resolve, `luthier refresh` failed out
of the box and every user had to pass `--registry-path`; and the only install
path was `cargo build --release` with a Rust 1.89 toolchain. The people this
tool is for are musicians.

The machinery for this exists: `.github/workflows/release.yml` builds a
statically linked musl binary on a `v*` tag, asserts it really is static
rather than trusting the target triple, packages it with the man page and
completions the binary generates itself, and publishes it — with a build
provenance attestation, and the SHA-256 GitHub shows for every asset —
under a versionless asset name so the README can name a `releases/latest`
URL that stays correct. Release notes come from `CHANGELOG.md`, and the job
refuses a tag that disagrees with the workspace version. The README's install
section leads with that download.

`v0.1.0` was tagged on 2026-09-27. The musl build passed on its first run,
the bench was signed on the maintainer's machine and the draft published. On
a clean root, the downloaded binary refreshed both benches with no flags,
verifying the signature, and installed and verified `lsp-plugins`. Signing
was later removed (see 2.2); from 0.2 on, a tag publishes with no step on
the maintainer's machine.

**Done when:** ~~the README's install section leads with the binary rather
than with cargo, a tagged `v0.1.0` publishes a static `luthier` binary for
linux-x86_64, and `luthier refresh` works on a clean machine with no flags~~.

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
carries only what OAS cannot: seven manifests. The three kits and the two
shadows pin an artifact to a version upstream will move past; the two
`external` engines have nothing of their own to go stale.

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

The data is the first half of the reason: of the bench's seven manifests,
the five with an artifact point at GitHub twice and at drumgizmo.org three
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

The manager's CI builds `--no-default-features` on every push. Since the
bench moved into this repository nothing else builds it that way, so that
job is what keeps the configuration from rotting silently.

---

## Phase 2 — Close the trust gaps — withdrawn

Neither item below was a live vulnerability. Both were places where the
security model rested on something narrower than it should, and both were
built, shipped in 0.1, and then taken out: the project trusts GitHub instead.

### 2.1 Verify the registry snapshot — done, then superseded

The document carrying every checksum was trusted on HTTPS alone.
`registry/provenance.rs` first answered that with trust on first use: each
bench's origin was pinned on first fetch, and a later change of host refused.

That only made sense while users could add benches. Once the sources became
fixed in the binary (see 3.2), an origin pin protected nothing a compiled-in
URL did not already fix, and would have locked every user out the day a
release moved a URL — so it was removed. What remains is an audit record of
each fetch: URL, digest, size and time. The key that signed it was recorded
too while 0.1 signed the bench; that field is now ignored.

### 2.2 Signature verification — withdrawn

0.1 signed the bench with Ed25519, a key kept off CI and compiled into the
manager, and verified it between the download and the extractor. That closed
the one gap HTTPS leaves: a compromised GitHub account, token or workflow
could otherwise publish a bench that points every user at a file of its
choosing. For a while it was moved to minisign's format so the signature
could be checked with standard tools.

It was then removed, by decision: the bench is trusted on GitHub, as the
binary is, and a release is a tag with no key to guard and no step on the
maintainer's machine. Homebrew and Scoop make the same trade. What the
manager still guarantees whatever the bench says — no destination in a
manifest, no scripts, checksums, one extraction policy — is in `SECURITY.md`
under *Trusting GitHub*, together with the risk this leaves. 0.1 clients
still require the signature and refuse the bench from later releases until
they are upgraded.

If it comes back, the history of `registry/signature.rs` has a minisign
implementation tested against minisign's own output.

---

## Phase 3 — Reach

### 3.1 Distribution packaging

An AUR package, a `.deb`, and ideally a Flatpak. The static binary in a
release asset exists (0.3), and so does the flake; neither reaches someone
who installs software through their distribution. `cargo install` is not a
distribution channel for this audience.

Every release now also carries a `.deb`, an `.rpm` and an install script
(unreleased, after 0.3). Downloaded from a release, none of them updates
itself; the AUR package and a Flatpak still would.

**Done when:** at least the AUR package exists and is referenced from the
README.

### 3.2 Third-party benches — withdrawn

0.1 shipped `bench add`, `bench remove`, `bench trust` and `bench untrust`, so
users could read collections of manifests beyond the default one. They were
removed: Luthier reads the Open Audio Stack registry and its own bench, and
nothing else. See *Deliberate ceilings*.

### 3.3 A `GitRegistry` backend

Branch tarballs were the right call for the MVP: no git dependency, and forges
publish them. A `gix`-based backend would make incremental refresh cheap.

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

### 3.5 Declarative application, and Nix

This is the use the whole design points at, so it is recorded as work rather
than as an aspiration.

`export` and `import` are already the declarative half: a file that
pins every version, and a command that installs what it names. What is missing
is *convergence*. Import is additive — it installs what the file lists and
leaves everything else alone — so the file describes a lower bound on the
installation rather than the installation itself. A declaration has to be able
to say what is not there, or applying it twice from different starting points
gives two different machines.

**Done when:** ~~`import --prune` removes packages the file does not name,
and importing the same file again reports no work~~. Done: a dependency
survives as long as something named needs it, and with `--prune` a file
naming nothing removes everything.

Then the Nix-facing work, in order of what unblocks what:

1. **A nixpkgs package.** The repository's flake builds one
   (`nix/package.nix`, the suite as its check phase), which is what 2 needed.
   Upstreaming it to nixpkgs is still open, and would let a user without the
   flake input have it.
2. **A Home Manager module — done.** `programs.luthier` (`nix/hm-module.nix`,
   documented in `docs/NIX.md`) writes the declared packages to a file in
   the export format and applies it during activation with `import`,
   `--prune` when asked, after `refresh` and `location set`. It runs luthier rather than
   building plugins as derivations, so there is one implementation of
   verification and placement; a failure warns rather than failing the
   switch. Tested against real Home Manager: install, a second switch doing
   nothing, pruning, a missing disk and a withdrawn version. This is the thing
   a Nix user actually wants; 3 below is an alternative to it, not a
   prerequisite.
3. **`luthier nix export`.** Emit a derivation set instead of installing one.
   Every artifact already carries a URL, a `sha256` and a size; no manifest can
   run a command; no manifest can name a destination. So a package translates
   to a fixed-output `fetchurl` plus a copy, with no impure step anywhere.
   Those properties came from the security model and happen to be exactly what
   a derivation requires — which is the reason to think this is a good fit
   rather than a fashionable one.

**Why it is worth doing at all**, stated plainly because the obvious objection
is that nixpkgs already has these plugins: it does. It carries the engines and
most of the plugin binaries, and it carries them well. What it does not carry,
and reasonably never will, is the content — kits, sample libraries and impulse
responses, redistributable but measured in gigabytes. Nor does Home Manager
have any notion of a plugin *set* for a DAW to scan. That is the gap, and it
is the same gap on every distribution; Nix is only where its absence is
felt most, because everything else on that machine is already declared.

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
`import` reproduces a set across the two — the point at which the vision
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

**Two sources, and users cannot add a third.** Luthier reads the Open Audio
Stack registry and its own bench, which corrects it, and nothing else. A
package with a downloadable release belongs upstream in OAS; the bench keeps
only what OAS cannot express. Third-party benches existed in 0.1 and were
removed: every source is one more publisher whose manifests can decide what
lands in a user's plugin directories, and one more host to trust, for
a need that contributing to OAS already meets.

**No environments.** 0.2 had named environments — `luthier env`, `--env`,
`LUTHIER_ENV` — and 0.3 removed them. Hosts see an environment's plugins
through `CLAP_PATH`, `VST3_PATH` and `LV2_PATH`, and the first two add to the
standard locations instead of replacing them, so CLAP and VST3 were never
isolated; the variables reach only a host started from that shell, not one
opened from a menu; and each environment kept its own copy of every sample
library. Plugins, presets and samples are not things a musician needs isolated
from each other. What environments were for — a set that stays put, and the
same set on another machine — is `export`, `import --prune`, pins and the Home
Manager module.

**No system-wide installation.** Everything is user-local; root is never
required and system directories are never written to. They are read to detect
`external` packages and are deliberately excluded from
`Layout::is_managed_location`, so no state file can direct a delete into them.

---

## Open questions

Things without an answer yet, recorded so they are not mistaken for oversights.

**How does an exported file handle a package that has left the registry?**
Import currently fails, which is right for reproducibility and unhelpful when a
project simply moved. A `--skip-missing` flag is the obvious answer; whether it
should also record what it skipped is not settled.

**What is the update story for a `pack`?** A pack resolves to dependencies, so
updating one means updating its members — but a pack's own version bump and its
members' bumps are different events, and `update` does not currently
distinguish them.

### Third-party registries — decided: none

This used to hold four entangled questions — what wins when two benches carry
one ID, whether third-party repositories carry a name prefix, what a user
types to add one, and what the thing is called. The first answer stands for
the two sources that remain: the bench is consulted first and wins, because
correcting OAS is what it is for. The rest went away with the decision not to
have third-party benches at all (see *Deliberate ceilings*).

The vocabulary stays. A *bench* is a collection of manifests; `luthier-extras`
is the one Luthier ships, built from `bench/` in this repository; "registry"
in code is the mechanism (`RegistryProvider`, `RegistryIndex`), which the Open
Audio Stack registry is read through as well.
