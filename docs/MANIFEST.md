# Manifest format

Schema v1. One TOML file per package, filed as `<id>.toml`.

The machine-readable schema is [`schemas/package-v1.json`](../schemas/package-v1.json),
generated from the Rust types. Point your editor at it for completion — Taplo
and the VS Code "Even Better TOML" extension both consume JSON Schema.

## A complete example

This is Surge XT, abridged to one release. It is shaped like a manifest this
registry once carried; the package now comes from the Open Audio Stack
registry, which lists the same tarball.

```toml
schema = 1

id = "surge-xt"                   # immutable; never contains a version
name = "Surge XT"
kind = "plugin"
category = "instrument"           # exactly one, from a closed list
tags = ["synthesizer"]            # free-form refinements

description = """
Hybrid synthesizer with wavetable, FM and subtractive engines."""

homepage = "https://surge-synthesizer.github.io/"
repository = "https://github.com/surge-synthesizer/surge"
documentation = "https://surge-synthesizer.github.io/manual-xt/"

authors = ["Surge Synth Team"]

[license]
kind = "open-source"
spdx = "GPL-3.0-or-later"

[[releases]]
version = "1.3.4"
release_notes = "https://github.com/surge-synthesizer/releases-xt/releases/tag/1.3.4"

[[releases.artifacts]]
target = { os = "linux", arch = "x86_64" }
source = { type = "http", url = "https://github.com/surge-synthesizer/releases-xt/releases/download/1.3.4/surge-xt-linux-1.3.4-pluginsonly.tar.gz" }
archive = "tar.gz"
size = 97294804
checksum = { sha256 = "dd431b75f5fa197c4bffa6ca27ca46970f0a94c834119bb1db7decdeec4c28db" }
provides = ["clap", "vst3"]
install = [
  { format = "clap", source = "Surge XT.clap", kind = "file" },
  { format = "clap", source = "Surge XT Effects.clap", kind = "file" },
  { format = "vst3", source = "Surge XT.vst3", kind = "bundle" },
  { format = "vst3", source = "Surge XT Effects.vst3", kind = "bundle" },
]
```

Note the ordering rule TOML imposes: every top-level key (`id`, `name`,
`description`, …) must appear **before** the first `[table]` header, or it will
be read as a key of that table instead. Keep the shape above.

Long prose uses TOML's line-continuation form — a `\` at the end of a line
inside `"""` swallows the newline and the indentation that follows — so the
file wraps at 80 columns while the value stays a single line.

## Top level

| Field | Required | Notes |
|---|---|---|
| `schema` | yes | `1`. A higher value than the client understands is an error. |
| `id` | yes | `[a-z0-9-]`, starting and ending alphanumeric, ≤64 chars. Never contains a version. Used as a path component, so the character set is a security boundary. |
| `name` | yes | Display name. |
| `kind` | yes | `plugin`, `library`, `preset-pack`, `application`, `pack`, `external`. |
| `category` | yes | Exactly one, from the closed list below. |
| `tags` | no | Free-form, lowercase, hyphenated. Search reads them. |
| `content` | `library` only | What only an engine can play: `sfz`, `sf2`, `drumgizmo`. See [Content and engines](#content-and-engines). |
| `description` | no | One or two sentences. Strongly recommended; search reads it. |
| `homepage`, `repository`, `documentation` | no | URLs. `repository` is the source, which is frequently *not* where artifacts are published. |
| `authors` | no | |
| `license` | yes | See below. |
| `releases` | yes, except for `external` | |
| `detect` | `external` only | How to recognise the package if already present. |
| `provisioning_hint` | `external` only | Shown when the package is missing. |

## Category and tags

`category` is the one field a browsing UI groups on, so its vocabulary is
**closed** — the validator rejects anything not on this list:

| Category | For |
|---|---|
| `instrument` | Produces sound: synthesizers, samplers, drum machines. |
| `effect` | Processes sound: reverb, EQ, dynamics, distortion. |
| `utility` | Analysers, meters and tools that are neither of the above. |
| `sample-library` | Sample content: multisampled instruments, drum kits, soundfonts. |
| `preset-pack` | Presets for another package. |
| `pack` | A curated set of dependencies and nothing else. |

Pick the one that describes what the package *is for*, not everything it
contains. A suite with both effects and a couple of instruments is an `effect`
suite; x42, whose own description leads with "metering, analysis and utility",
is `utility` rather than `effect`.

`tags` carries everything finer grained, and stays open because the useful
descriptors are long-tailed:

```toml
category = "instrument"
tags = ["synthesizer", "fm"]
```

Do not repeat a category name as a tag. If a package genuinely spans two
categories, that is a signal to check whether upstream ships them as separate
downloads.

Adding a category is a change to `luthier-manifest`, deliberately: it is the
one axis the whole registry has to agree on. A client built before the addition
still reads such a manifest (see *Forward compatibility*), it just declines to
validate it.

## Licence

```toml
[license]
kind = "open-source"        # open-source | freeware | proprietary | custom
spdx = "GPL-3.0-or-later"   # required when kind is open-source
name = "Vendor EULA"        # required when kind is custom
url = "https://..."         # optional
```

Free of charge is not the same as open source. `kind = "open-source"` requires
an SPDX expression whose licences are OSI-approved or FSF-libre;
`CC-BY-NC-4.0` is valid metadata under `freeware` and rejected under
`open-source`.

Expressions are parsed against the official SPDX list, so `GPL3` fails and so
does the deprecated `GPL-3.0` — use `GPL-3.0-only` or `GPL-3.0-or-later`
according to what the project's own notices say. Record what upstream states;
do not infer.

## Releases

```toml
[[releases]]
version = "1.3.4"                       # strict semver
release_notes = "https://..."           # optional
yanked = "reason"                       # optional; excluded from resolution
dependencies = ["sfizz"]                # or the table form, below
optional_dependencies = []
```

A dependency is either a bare ID or a table with a version requirement:

```toml
dependencies = [
  "sfizz",
  { id = "engine", version = ">=1.2, <2.0" },
]
```

File order does not matter: releases are sorted by version, so resolution does
not depend on how the manifest was written.

## Artifacts

A release carries a *list*, because one archive often provides several formats
and a project may publish more than one usable archive per target.

```toml
[[releases.artifacts]]
target = { os = "linux", arch = "x86_64" }
```

| Field | Required | Notes |
|---|---|---|
| `target` | yes | `{ os, arch }`. Currently `linux` with `x86_64`/`aarch64`. |
| `source` | yes | `{ type = "http" \| "file", url = "..." }`. |
| `archive` | yes | `tar.gz`, `tar.xz`, `zip`, `7z`, `none`. |
| `size` | no | Bytes. Used for progress; the checksum is what decides. |
| `checksum` | yes | `{ sha256 = "..." }`, 64 hex characters. |
| `provides` | no | Formats this artifact delivers. |
| `install` | yes | What to take out of it. |

When several artifacts match a target, the first is used. Put the one users
should get first — for Surge XT that is the 92 MiB plugins-only tarball, not
the 333 MiB full build.

## Install rules

```toml
install = [
  { format = "clap", source = "Surge XT.clap", kind = "file" },
  { format = "vst3", source = "Surge XT.vst3", kind = "bundle" },
  { format = "clap", source = "CLAP/Fire.clap", kind = "file", rename = "Fire.clap" },
]
```

- `format` selects the destination root (`clap` → `~/.clap`, `vst3` → `~/.vst3`).
- `source` is a path inside the archive: relative, no `..`, no absolute paths.
- `kind` is `file` for CLAP, `bundle` for VST3 and LV2. A mismatch is rejected.
- `rename` optionally changes the leaf name. A single filename, never a path.
- `allow` accepts a warning this rule is knowingly at odds with. See below.

**There is no `destination` field.** The destination is derived from the format
and the leaf name. See [SECURITY.md](../SECURITY.md) for why.

### Accepting a warning

The registry's CI runs `validate --strict`, which makes every warning fatal. A
warning that is right in general is occasionally wrong for one real package,
and the fix must not be to stop running `--strict` — that would silence every
future warning too. So a rule can accept one by name:

```toml
# The 3D rendering library the CLAP loads at runtime. Upstream keeps it beside
# the plugin; a host scanning for *.clap ignores it.
{ format = "clap", source = "CLAP/liblsp-r3d-glx-lib.so", kind = "file",
  allow = ["file-extension"] },
```

`file-extension` is the only value: the installed name does not carry its
format's conventional extension. Nothing in `allow` can relax a check on where
a file may be written — these name conventions, not safety.

An allowance that silences nothing warns in turn, so one written for a rule
that later changed does not sit there accepting a real warning years on.

There is also no way to run a command. Installation is declarative: extract,
copy, create a directory, record metadata.

`install` is required. An artifact that declares no rules is not a release for
the target, so it cannot be installed at all — the manager will not guess what
to copy out of an archive a manifest failed to describe.

The one exception is not authorable here. A registry provider reading a source
that carries no rules of its own — the Open Audio Stack registry says which
formats an archive holds, never which entry is which — marks its artifacts
`derive_install`, and the rules are read from the verified archive by the same
code behind `luthier-registry inspect`. `validate` refuses the field in a
manifest: rules written here are rules someone reviewed, and that is what makes
this registry win a collision with a derived one.

## External packages

For software with no redistributable Linux binary:

```toml
kind = "external"
provisioning_hint = """
Install sfizz from your distribution or build it from source."""

[[detect]]
format = "vst3"
name = "sfizz.vst3"

[[detect]]
format = "lv2"
name = "sfizz.lv2"
```

Any one matching rule satisfies the dependency. An external package must
declare no artifacts — that would contradict the promise never to download it.

## Packs

Metadata only, resolving to dependencies. No artifacts, no duplicated binaries.

```toml
id = "foss-studio"
kind = "pack"
category = "pack"

[[releases]]
version = "1.0.0"
dependencies = ["surge", "dexed", "dragonfly-reverb", "fire"]
```

## Content and engines

A sample library installs perfectly and still does nothing without an engine
to play it. It says what it holds, not what plays it:

```toml
id = "crocellkit"
kind = "library"
category = "sample-library"
content = ["drumgizmo"]
```

Which packages play each kind of content is registry data, in `engines.toml`
at the root of a bench:

```toml
[[engine]]
package = "drumgizmo"            # detect rules come from its own manifest
plays = ["drumgizmo"]

[[engine]]
package = "drumcraker"           # from another registry, so detect goes here
plays = ["drumgizmo"]
detect = [{ format = "vst3", name = "DrumCraker.vst3" }]
```

Before downloading a library, the manager looks for any one engine for every
value in `content`: found by detect rules in the managed or system plugin
directories, recorded as installed, or being installed in the same command.
With none it warns, lists them, and refuses. Content no configured registry
names an engine for is refused the same way, since nothing can vouch that one
is present.

This is not a dependency, deliberately. A dependency names one package, and a
kit that depended on `drumgizmo` would refuse a user who plays it in DrumCraker.

`content` uses the Open Audio Stack registry's `contains` vocabulary, so a
library read from there needs no translation: its `contains` becomes its
`content`. A package that also ships a plugin plays its own content and
declares none — `validate` refuses `content` outside `kind = "library"`, and
`luthier-registry validate` refuses a value no engine in `engines.toml` plays.

## Forward compatibility

Fields this client does not recognise are kept and logged rather than treated
as errors, so an older `luthier` keeps working against a newer registry. The
same holds for unrecognised *values*: a category or format added later parses
and round-trips verbatim, and is refused during validation with a message
naming what this build knows.

Registry CI runs in strict mode, so an unrecognised field there fails the pull
request — which is what you want when it is a typo.

Room is reserved for signed metadata; no signature is checked today.
