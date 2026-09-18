# Environments, export and import

An environment is a named set of installed software: its own plugin
directories, its own state, its own libraries. Registry snapshots and the
downloaded-artifact cache stay shared, so a second environment installing the
same plugin re-extracts from cache rather than downloading again.

```console
$ luthier env create mixing
$ eval "$(luthier env activate mixing)"
$ luthier install surge             # lands in the environment
$ luthier env deactivate
```

Activation exports `LUTHIER_ENV`, which the manager reads, and the three plugin
search-path variables, which hosts read. `--env <name>` does the same for a
single command without touching the shell.

There is no "current environment" file. A stored pointer would let one terminal
change what another is about to install into, so selection lives in the
environment, as it does for a virtualenv.

Which means the answer to "where am I?" is a question about this shell, and
`luthier env show` gives it: the environment in use and where it lives, or that
the default one is. `luthier env path [name]` prints a directory and nothing
else, so it composes — `ls "$(luthier env path mixing)"` — and defaults to the
active environment when no name is given.

One asymmetry is worth knowing, because it comes from the plugin formats rather
than from this tool. `LV2_PATH` *replaces* a host's default search path, so an
environment can fully determine which LV2 bundles a host sees. `CLAP_PATH` and
`VST3_PATH` only *extend* the standard locations, so `~/.clap` and `~/.vst3`
stay visible alongside the environment's own.

## Moving a setup to another machine

```console
# on the old machine
$ luthier env export -o studio.toml

# on the new one
$ luthier env import studio.toml
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
luthier = "0.1.0"
exported = "2026-09-06T21:07:05.487622056Z"
environment = "mixing"
pinned = true

[[package]]
id = "dragonfly-reverb"
version = "3.2.10"
registry = "default"
reason = "explicit"

[[package]]
id = "sfizz"
version = "1.2.3"
registry = "default"
reason = "dependency"
pin = "1.2.3"
```

| Field | Meaning |
|---|---|
| `meta.schema` | Format revision. A file from a newer revision is refused rather than half-understood. |
| `meta.pinned` | Whether `version` fields are authoritative. |
| `meta.environment` | Where it came from. Informational — import never switches environments on the strength of a file's contents. |
| `package.reason` | `explicit` packages are what import asks for by name; `dependency` entries are recorded so their versions reproduce, but stay dependencies. |
| `package.pin` | A pin the user had applied, reapplied after the install. |

Packages are sorted by ID, so two exports of the same installation are
byte-identical and the file diffs cleanly in version control.

### What it deliberately does not record

No absolute paths, no plugin directories, no destination of any kind. Where
things land is the receiving machine's business, decided by its own layout. A
file that could name destinations would be the same arbitrary-write primitive
that manifests are forbidden from being — see [SECURITY.md](../SECURITY.md).

### Reproducible or portable: pick one

```console
$ luthier env export --loose -o studio.toml
```

`--loose` records package identities without versions. The result installs the
current release of each package on whatever machine reads it: portable across
registry states, but not reproducible. Use it for "give me my usual set", and
the default for "give me exactly what I had".

A pinned import fails rather than substituting. If the registry no longer
carries a version the file names, you get:

```
error: surge-xt 1.3.4 is not in the registry, so the environment cannot be reproduced
hint: Run `luthier refresh`; if the version is genuinely gone, re-export from a
      machine that still has it, or import with a `--loose` file.
```

That is the intended behaviour: an import that quietly installed something else
would not have reproduced anything, and you would find out in the middle of a
session rather than at import time.

### Importing into a specific environment

Import installs into whichever environment is selected, exactly like `install`:

```console
$ luthier --env mixing env import studio.toml
$ eval "$(luthier env activate mixing)" && luthier env import studio.toml
```

The `environment` field in the file is not consulted for this. A file that
could redirect where it installs would be deciding something the person running
the import should decide.

## Cross-machine and cross-OS notes

Everything above works between Linux machines today. The manifest schema models
`macos` and `windows` targets and the resolver handles them, but the installer
layouts and the registry's artifacts are Linux-only for now, so an environment
file moved to another OS will fail to resolve rather than mis-install. Making
that work is a matter of adding the platform layouts and the artifacts, not of
changing this format.
