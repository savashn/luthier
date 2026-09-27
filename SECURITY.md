# Security model

Everything downloaded is treated as hostile until proven otherwise. This
document states what is trusted, what is not, and what the code does about it.

## Trust boundaries

| Input | Trusted? | Why |
|---|---|---|
| Registry manifests | Reviewed, not trusted | Manifests are reviewed in pull requests, but a manifest still cannot name a destination path or run a command |
| Registry sources | Fixed in the binary | Luthier reads the Open Audio Stack registry and its own bench, and nothing else; users cannot add a source |
| The bench snapshot | Only if signed with the built-in key | Verified with minisign against a key compiled into the manager, before it is extracted |
| The Open Audio Stack index | HTTPS alone | Its publisher signs nothing, so the bench — which is signed — is what corrects it |
| Downloaded artifacts | Never | An artifact is bytes from the internet; the checksum only proves it is *the* expected bytes, not that they are safe |
| Local state file | Structurally, not blindly | Removal re-checks that every path it is about to delete lies inside a managed directory |
| The user's existing plugins | Never modified | Anything Luthier did not install is left alone |

## No install scripts

There is no way for a package to run a command. The manifest is declarative and
the only operations available are: extract, copy a file, copy a bundle, create a
directory, record metadata. This is not a policy that can be relaxed by a
manifest field, because no such field exists.

## Destinations are derived, not declared

The specification sketched install rules carrying an absolute destination:

```toml
[install.clap]
destination = "~/.clap/Surge XT.clap"   # NOT the format used here
```

That would hand any registry contributor an arbitrary-write primitive against
the user's home directory. Instead a rule names only what to take out of the
archive and which format it is:

```toml
install = [
  { format = "clap", source = "Surge XT.clap", kind = "file" },
]
```

The destination is computed from the format's root plus the leaf name. An
optional `rename` may change the leaf name and is validated to be a single path
component. A manifest cannot express a path outside the plugin directory for
its format, and the installer asserts this again before writing.

## Rules may be derived, but never invented

A source can carry the bytes and their checksum without carrying the rules. The
Open Audio Stack registry is one: it says which *formats* an archive holds, not
which *entry* is which. Sorting entries into per-format directories needs the
missing half, and the archive is the only place it exists — so a provider may
mark an artifact `derive_install`, and the rules are read from the extracted
tree instead.

The distinction that matters is not declared-versus-derived. It is this:

- **A manifest may not leave the manager guessing.** `validate` refuses
  `derive_install` in a manifest, and an artifact that declares no rules and
  does not derive them is not a release for the target at all. A curated bench
  earns its precedence by carrying rules a person looked at.
- **Derivation reads bytes whose checksum has already been verified**, by the
  same code `luthier-registry inspect` shows a contributor
  (`luthier-core::install::derive`). One implementation, so the tool's
  suggestion and the installer's behaviour cannot drift.

Nothing above is weakened by it. Derivation produces the same `InstallRule`
values a manifest would, they go through the same `install::plan`, and the
destination is still computed from the format's root — it is never read from
the archive, the manifest or the source. Recognition is narrow on purpose: a
format's conventional extension *and* the shape that format requires on disk,
so a `ProM.clap` that ships as a directory is not a CLAP. Anything unrecognised
is not installed, and an archive yielding nothing is an error rather than a
package recorded as installed with no files.

## Archive extraction

Extraction policy lives in one place, `crates/luthier-core/src/archive/safe.rs`, and
applies identically to every container format. The per-format modules decide
only what entries exist. Notably, neither `tar::Archive::unpack` nor
`ZipArchive::extract` is used — those apply their own policy, and there should
be exactly one to audit.

Refused outright:

- Absolute paths, `..` components, root or drive prefixes
- Interior `.` components, empty components, backslashes, NUL bytes
- Symbolic links, of any target
- Hard links naming anything this extraction has not already written
- Device nodes, FIFOs, sockets, sparse files
- Duplicate entry names
- Entries whose parent directory is a symlink

A *leading* `./` is stripped rather than refused: GNU tar writes it for any
archive built with `tar -C dir .`, which is how Surge XT and many other real
releases are packaged. Stripping happens before validation, so it cannot be used
to smuggle a `..` past the check.

Permission bits are reduced to `0o755` for directories and executables and
`0o644` otherwise. setuid, setgid and sticky bits never reach the disk.

Limits bound decompression bombs: maximum entry count, maximum total expanded
size, maximum single-entry size, maximum path depth and length.

Files are created with `O_CREAT|O_EXCL`, which does not follow a symlink at the
final path component, and parent directories are created by us rather than with
`create_dir_all`, which would walk through an existing link.

Hard links are the one link type not refused outright, because real packages
need them: DPF-Plugins ships the presets repeated across its plugins that way.
A tar hard link is accepted only when it names an entry the same extraction has
already written — which is therefore inside the root and already vetted — and
is then materialised as an independent copy. The link target goes through the
same name rules as any entry rather than a second, weaker check, and one naming
a path the archive never produced is refused rather than resolved against the
filesystem. Copying rather than linking means no two extracted paths ever share
an inode.

## Verification

An artifact is downloaded to a temporary name, hashed as the bytes stream past,
and only renamed into the cache once the digest matches the manifest. A mismatch
deletes the partial file and aborts. Nothing is extracted, let alone installed,
before this succeeds.

The cache is content-addressed: an entry's filename *is* its SHA-256. A cache
hit is still re-verified, so a corrupted or tampered cache entry is discarded
and refetched rather than trusted.

## Registry sources

An artifact is verified against a checksum in a manifest, so the document
carrying that checksum is the weakest link in the chain. Luthier reads exactly
two, and both are fixed in the binary:

1. **Luthier's own bench** (`luthier-extras`), built from `bench/` in this
   repository and published as the `bench.tar.gz` asset of each release. It
   is consulted first, so it wins any package ID both sources carry — which is
   how it corrects the other one.
2. **The Open Audio Stack registry**, which carries almost every package.

There is no command to add a source, and `config.json` cannot name one: a
`registries` list written there by 0.1 is ignored. `--registry-path` replaces
both with a local directory for one command; it is hidden, and exists for
developing the bench.

`registry/provenance.rs` records, for each source, the URL, the SHA-256 and
size of what arrived, when, and the key that signed it. The record is for
audit and enforces nothing. (0.1 also pinned each source's origin and key on
first use, because users could add sources; with the URLs and the key fixed in
the binary a pin protects nothing and would lock everyone out the day a
release moved either.)

## Bench signatures

Each release publishes a detached signature beside the bench, at the
snapshot's own URL with `.minisig` on the end. The format is
[minisign](https://jedisct1.github.io/minisign/)'s, byte for byte: Ed25519
over the snapshot's BLAKE2b-512 (minisign's prehashed `ED` form), plus a
signed trusted comment. The manager verifies it **between the download and the
extractor**, so a snapshot nothing vouched for is never opened, and a refusal
always leaves the previous snapshot in place.

The key is built into the manager (`DEFAULT_BENCH_KEY` in `config.rs`) and is
the only one accepted:

```text
RWTIP+H7i3W+zON5bZiS8gDxRaW++7Qhpm+51rpaaK/mJGBE37pxapmq
```

A minisign signature names its key only by ID, so nothing fetched from the
forge can say which key to believe: an attacker who controls the account can
replace the bench and its signature, but not the key in a binary users already
have. Anyone can check a bench without trusting the manager at all:

```console
$ minisign -Vm bench.tar.gz -P RWTIP+H7i3W+zON5bZiS8gDxRaW++7Qhpm+51rpaaK/mJGBE37pxapmq
```

The cost of a built-in key is that a release without a signature is a bench
nobody can refresh. The release workflow therefore publishes a draft —
invisible to `releases/latest` — and the signature is made on the
maintainer's machine, with a key CI never sees, before the draft is published.

Three refusals follow, and only one of them can be overridden:

| | |
|---|---|
| No signature published | `refresh --allow-unsigned` accepts it for **one run**; the next refresh asks again. |
| A signature that does not verify | Refused. No flag relaxes this: it is a claim that did not hold up, not an absent one. |
| A signature from any other key | Refused. A rotation reaches users as a release carrying the new key. |

An unreadable signature file is refused rather than treated as no signature,
or publishing garbage would be a way to turn verification off.

Releases also publish `bench.tar.gz.sig`, the format 0.1 reads, so 0.1 clients
keep refreshing; nothing newer reads it. The signing side is
`luthier-registry keygen` and `luthier-registry sign`, which write minisign's
files (stock `minisign` works too); the procedure, including rotation, is in
[docs/REGISTRY.md](docs/REGISTRY.md#signing-a-snapshot).

## Release provenance

The binary and the bench are also covered by a GitHub build provenance
attestation, made by the release workflow through Sigstore with the
workflow's own identity. It says the file was built by this repository's
`release.yml`, from which commit:

```console
$ gh attestation verify luthier-x86_64-linux.tar.gz -R savashn/luthier
```

The two answer different questions. The attestation says CI built it, with no
key for anyone to lose; the bench's minisign signature says the maintainer
published it, with a key CI never holds.

## Installation

Installs are journalled. Each item is staged in its final directory (so the
rename is same-filesystem and therefore atomic), the journal is flushed, and
only then is the rename performed. A failure part-way replays the journal
backwards; a journal left behind by a killed process is replayed on the next
run. The outcome is always the complete new state or the complete previous one,
never a half-installed package.

Before writing, each artifact is validated as what it claims to be: a CLAP must
be an ELF64 shared object for this machine, a VST3 must be a bundle containing
`Contents/<arch>-linux/*.so`. This catches a mislabelled artifact at install
time rather than leaving the user with a plugin that silently never loads.

An existing file at a destination aborts the install unless Luthier installed
it. Files installed by hand, by a distribution package or by a vendor installer
are never replaced.

## Removal

Only paths recorded at install time are deleted, and each is re-checked against
the managed directories first, so a corrupted or hand-edited state file cannot
turn removal into arbitrary deletion. "Inside" means strictly below a root by
plain names: a root itself (`~/.clap`) is never a deletion target, and a path
with a `..` in it is refused even though it textually starts with a root. A
bundle's recorded contents are checked the same way, because removal joins
them onto the bundle and deletes the result.

Anything changed, added or replaced since installation is reported and kept.
A bundle is decided file by file, so one edited preset keeps one file rather
than the whole bundle, and no symbolic link is followed: a directory inside a
bundle that has been replaced by a link is left alone rather than emptied
through it. Replacing a package — `update`, or an install that upgrades — is
refused before the download when it would delete such changes, until
`--force` says to go ahead.

## Not yet implemented

- **Per-manifest signatures.** A snapshot is verified as a whole (see
  *Registry signatures*); an individual manifest inside one is not signed
  separately, though the format reserves room for it (§14). In practice the
  snapshot signature covers every manifest it carries, so what is missing is
  the ability for one package's author to sign their own entry independently
  of the bench that publishes it.
- **System-wide installation.** Everything is user-local; root is never
  required and system directories are never written.

  System plugin directories — `/usr/lib/lv2` and the rest of the conventional
  search paths, plus whatever `CLAP_PATH`, `VST3_PATH` and `LV2_PATH` name —
  are *read* when detecting `external` packages, because that is the only place
  a distribution-provided engine can be. They are deliberately excluded from
  `Layout::is_managed_location`, which is what the uninstaller gates on, so no
  state file — corrupted, hand-edited or hostile — can direct a delete into
  them. `Layout::rooted_at` starts with no system roots at all, which keeps the
  test suite independent of the machine running it.

System directories are read for detection only, never for installation, and
`--no-system-plugins` turns even that off.

## Archive containers

`.tar.gz`, `.tar.xz`, `.zip` and `.7z` all go through the same
[`SafeExtractor`]: each container module decides only *what entries exist*, and
one policy decides whether any of them may touch the disk. The
malicious-archive corpus exercises every container against the same attacks, so
a container added later could not bring a policy of its own.

## Environment names

`--env <name>` and `LUTHIER_ENV` become a path segment under the data
directory, so the name is validated before any path is built from it: letters,
digits, `.`, `-` and `_`, no leading dot, at most 64 characters. That rejects
`..`, `../escape`, absolute paths and separators. `env remove` re-checks that
the directory it is about to delete recursively is inside the environments
directory, because a validated name is not a reason to skip the check on a
recursive delete.

## Reporting

Please report security issues privately to the maintainers rather than in a
public issue.
