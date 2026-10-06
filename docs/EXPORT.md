# Export and import

A setup can be written to a file and rebuilt from it, version for version, on
another machine — or on the same one after a reinstall:

```console
# on the old machine
$ luthier export -o studio.toml

# on the new one
$ luthier refresh
$ luthier import studio.toml
```

That is the whole point of the format: a creative setup should survive a move,
not have to be rebuilt from memory. Export records the exact version of every
package — including the ones that arrived as dependencies — and import feeds
those versions to the resolver as hard requirements, so the same set comes back
rather than "whatever is newest today".

An exported file looks like this:

```toml
[meta]
schema = 1
luthier = "0.4.0"
exported = "2026-09-06T21:07:05.487622056Z"
pinned = true

[[package]]
id = "dragonfly-reverb"
version = "3.2.10"
registry = "oas"
reason = "explicit"

[[package]]
id = "sfizz"
version = "1.2.3"
registry = "luthier-extras"
reason = "dependency"
pin = "1.2.3"
```

| Field | Meaning |
|---|---|
| `meta.schema` | Format revision. A file from a newer revision is refused rather than half-understood. |
| `meta.pinned` | Whether every package names a version. A `version` is a hard requirement wherever one is given, so a hand-written file with `pinned = false` can hold some packages at a version and let the rest follow the registry. |
| `package.registry` | Where the manifest came from. Informational, and may be left out. |
| `package.reason` | `explicit` packages are what import asks for by name; `dependency` entries are recorded so their versions reproduce, but stay dependencies. |
| `package.pin` | A pin the user had applied, reapplied after the install. |

Packages are sorted by ID, so two exports of the same installation are
byte-identical and the file diffs cleanly in version control. A file written
by 0.2, which also named the environment it came from, still imports; that
field is ignored.

## What it deliberately does not record

No absolute paths, no plugin directories, no destination of any kind. Where
things land is the receiving machine's business, decided by its own layout and
its own `luthier location` settings. A file that could name destinations would
be the same arbitrary-write primitive that manifests are forbidden from being —
see [SECURITY.md](../SECURITY.md).

## Reproducible or portable: pick one

```console
$ luthier export --loose -o studio.toml
```

`--loose` records package identities without versions. The result installs the
current release of each package on whatever machine reads it: portable across
registry states, but not reproducible. Use it for "give me my usual set", and
the default for "give me exactly what I had".

A pinned import fails rather than substituting. If the registry no longer
carries a version the file names, you get:

```
error: surge 1.3.4 is not in the registry, so the exported set cannot be reproduced
hint: Run `luthier refresh`; if the version is genuinely gone, re-export from a
      machine that still has it, or import with a `--loose` file.
```

That is the intended behaviour: an import that quietly installed something else
would not have reproduced anything, and you would find out in the middle of a
session rather than at import time.

## Making an installation match a file

An import only adds: it installs what the file names and leaves everything
else alone. With `--prune` it also removes every installed package the file
neither names nor needs, so the installation ends up exactly as the file
describes, and importing the same file again reports nothing to do:

```console
$ luthier import --prune studio.toml
```

A dependency survives as long as something the file names needs it. With
`--prune`, a file naming no packages removes everything. Removal keeps any
file changed since it was installed, as `luthier remove` does. This is what the
Home Manager module runs when its `prune` option is on; see
[Nix and Home Manager](NIX.md).

## Why there are no environments

Luthier 0.2 had named environments — `luthier env create`, `--env`,
`LUTHIER_ENV` — each with its own plugin directories. They were removed in
0.3, because they did not deliver what they promised:

- A host finds an environment's plugins through `CLAP_PATH`, `VST3_PATH` and
  `LV2_PATH`, and the first two only *add* to the standard locations. Every
  CLAP and VST3 plugin in `~/.clap` and `~/.vst3` stayed visible in every
  environment.
- Those variables reach only a host started from a shell that set them, not
  one opened from a menu.
- Each environment kept its own sample libraries, so a kit used in two was on
  disk twice.

What they were for — a set of plugins that stays put for a project, and the
same set on another machine — is what `export`, `import`, `luthier pin` and a
version in the Home Manager module do. Environment directories a 0.2
installation created are left where they were, under
`~/.local/share/luthier/envs`; nothing reads them, and deleting them is up to
you.

## Cross-machine and cross-OS notes

Everything above works between Linux machines today. The manifest schema models
`macos` and `windows` targets and the resolver handles them, but the installer
layouts and the registry's artifacts are Linux-only for now, so an exported
file moved to another OS will fail to resolve rather than mis-install. Making
that work is a matter of adding the platform layouts and the artifacts, not of
changing this format.
