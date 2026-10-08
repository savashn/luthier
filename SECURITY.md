# Security model

Everything downloaded is treated as hostile until proven otherwise. This
document states what is trusted, what is not, and what the code does about it.

## Trust boundaries

| Input | Trusted? | Why |
|---|---|---|
| Registry manifests | Reviewed, not trusted | Manifests are reviewed in pull requests, but a manifest still cannot name a destination path or run a command |
| Registry sources | Fixed in the binary | Luthier reads the Open Audio Stack registry and its own bench, and nothing else; users cannot add a source |
| The bench and the Open Audio Stack index | HTTPS and the forge | Neither is signed. Whoever can publish a release of this repository, or change the OAS site, decides what the checksums say |
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

1. **Luthier's own bench** (`extras`), built from `bench/` in this repository
   into the binary itself, so it is exactly as trustworthy as the binary,
   under the same provenance attestation, and nothing is fetched for it. (0.4
   and earlier fetched it as the `bench.tar.gz` asset of the latest release,
   which releases still publish for them.) It is consulted first, so it wins
   any package ID both sources carry — which is how it corrects the other one.
2. **The Open Audio Stack registry**, which carries almost every package.

There is no command to add a source, and `config.json` cannot name one: a
`registries` list written there by 0.1 is ignored. `--registry-path` replaces
both with a local directory for one command; it is hidden, and exists for
developing the bench.

`registry/provenance.rs` records, for each source, the URL and the SHA-256 and
size of what arrived, when. The record enforces nothing. It also says when the
source was last asked, which decides when a command refreshes the Open Audio
Stack index on its own (once a day), and keeps the `ETag` and `Last-Modified`
the server sent, which a refresh sends back so that an unchanged index is not
downloaded again. They are sent only while the snapshot on disk hashes to what
the record says arrived, from the same URL. So a change to that index reaches
every user within a day without a `refresh`, exactly as it would with one.
(0.1 also pinned each source's origin on first use, because users could add
sources; with the URLs fixed in the binary a pin protects nothing and would
lock everyone out the day a release moved one.)

## Trusting GitHub

Nothing the manager reads is signed. The bench is part of the binary, so it is
trusted on HTTPS and on GitHub exactly as the binary is, and the Open Audio
Stack index on HTTPS and on the site that serves it. This is a deliberate
choice, and it has a consequence worth stating plainly: **anyone who can
publish a release of this repository can publish a binary whose bench points
at any file with a matching checksum, and every user who updates installs it
on their next `install`.** That means a compromised GitHub account, a leaked
token with write access, or a compromised release workflow. Homebrew and Scoop
make the same trade; apt and pacman do not. Clients from 0.2 to 0.4 fetch
the bench on its own, as the `bench.tar.gz` of the latest release: as long as
releases publish it, such a release also reaches every one of them on their
next `refresh`, whether they update or not.

0.1 signed the bench with a key kept off CI and compiled into the manager,
which closed that gap. It was removed to keep releases a single tag with no
key to guard; 0.1 clients still require that signature and refuse the bench
from any later release until they are upgraded.

What still holds whatever the bench says: a manifest cannot name a
destination or run a command, every artifact is checked against the checksum
its manifest gives, and extraction goes through one hardened policy. A
malicious bench can make a user install a malicious *plugin*; it cannot make
the installer write outside the plugin and library directories.

`luthier update --self` trusts GitHub the same way, and more directly: the
SHA-256 it checks a new Luthier against comes from GitHub's API, for the same
release. It catches a download that was corrupted or swapped on the way; it
cannot catch a release published by someone who should not have been able to,
and that release replaces the manager itself — through `sudo` for the .deb
and the .rpm. It does not check the provenance attestation below.

## Release provenance

Every file a release publishes — each architecture's tarball, .deb and .rpm,
the install script and the bench — is covered by a GitHub build provenance
attestation, made by the release workflow through Sigstore with the
workflow's own identity. It says the file was built by this repository's
`release.yml`, from which commit:

```console
$ gh attestation verify luthier-$(uname -m)-linux.tar.gz -R savashn/luthier
```

The manager does not check it; it is for a user who wants to. It rests on
the same trust as everything else here — an attacker who can run this
repository's workflow can produce an attested file too.

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

- **Signatures.** Neither the bench nor an individual manifest is signed
  (see *Trusting GitHub*), though the manifest format reserves room for it
  (§14).
- **System-wide installation.** Everything is user-local; root is never
  required and system directories are never written. The exception is
  Luthier itself, where it was installed as root. `luthier update --self` of
  a Luthier from the .deb or .rpm runs apt, dnf or zypper through `sudo`, as
  installing the new package by hand would: root copies the verified
  download into a directory only root can write and checks its SHA-256 again
  there before the package manager reads it, so the user-writable cache it
  came from cannot change it in between. One the install script put in
  `/usr/local` is replaced only by a user who can write there, which is
  root, run that way by hand.

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

## Reporting

Please report security issues privately to the maintainers rather than in a
public issue.
