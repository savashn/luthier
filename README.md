# Luthier

A package and environment manager for Linux audio software.

Linux audio plugins are scattered across GitHub releases, GitLab, vendor sites
and distribution repositories, in a handful of archive formats and several
plugin formats. Installing one usually means finding the project, finding the
release, downloading an archive, working out which directory each format
belongs in, and copying files by hand.

This replaces that with:

```console
$ luthier search synth
ID           NAME              VERSION  CATEGORY    TAGS
dexed        Dexed             1.0.1    instrument  synthesizer, fm
surge        Surge XT          1.3.4    instrument  synthesizer

$ luthier install surge
$ luthier list
$ luthier remove surge
```

## Status

Linux x86_64, CLAP, VST3, LV2 and sample libraries, from `.tar.gz`, `.tar.xz`,
`.zip` and `.7z` archives. DAW-agnostic: it installs into the directories hosts
already scan and knows nothing about any particular DAW. The manifest schema
models macOS and Windows targets, but the layouts and registry artifacts are
Linux-only for now. Installs into your home directory; root is
never required and system directories are never written to. Distribution and
container-provided plugins are detected in the conventional system search paths
(and whatever `CLAP_PATH`, `VST3_PATH` and `LV2_PATH` name), never modified.

## What it will not do, and why some plugins are missing

Installing a package here means: download it, check it against the checksum in
its manifest, extract it, and copy files into place. That is the complete list.
There is no field in a manifest that runs a command, no post-install hook, no
build step — and none will be added.

The reason is that a registry is a pull request away from every user's machine.
If a manifest could run a command, then merging one would mean running a
stranger's code on every computer that installs it. Nothing about reviewing a
pull request carefully makes that safe enough. So the manager cannot execute
anything, and there is nothing to review for: the worst a merged manifest can
do is put a file in a plugin directory. See [SECURITY.md](SECURITY.md).

Two consequences follow, and they explain most of what is not here.

**Software that installs itself is out of reach.** A `.deb`, `.rpm` or `.exe`
is a program that unpacks itself and runs scripts as it goes. Running one is
exactly what this manager will not do — and running it as root, which those
formats expect, doubly so.

**Software distributed only by distributions is out of reach.** Guitarix, Calf,
x42-plugins and many other excellent projects publish source, and their binaries
are built by Debian, Arch, Fedora and the rest. Those binaries are real, but
they are built against one distribution's library versions and belong in
`/usr/lib`. Copying Debian's build into `~/.lv2` on Arch produces a file that
installs cleanly and then fails to load, which is the one outcome this manager
is designed never to produce. Your distribution's package manager does this job
properly; there is nothing to gain from doing it badly here.

What is left is what upstream publishes as a portable archive — a build that
carries what it needs and runs anywhere. Surge XT, Dexed, LSP Plugins and
several hundred more do exactly that, and those are the packages you will find.

So: if a plugin is missing, `apt install` or `pacman -S` is usually the answer,
and that is not a workaround. It is the other half of a division of labour.

## Install

One statically linked binary, no runtime to install and no toolchain to build
it with:

```console
$ curl -LO https://github.com/luthier/luthier/releases/latest/download/luthier-x86_64-linux.tar.gz
$ curl -LO https://github.com/luthier/luthier/releases/latest/download/luthier-x86_64-linux.tar.gz.sha256
$ sha256sum -c luthier-x86_64-linux.tar.gz.sha256
$ tar xzf luthier-x86_64-linux.tar.gz
$ install -Dm755 luthier-*/luthier ~/.local/bin/luthier
```

Check the checksum rather than skipping it. A manager whose whole job is
verifying what it downloads should be worth the same courtesy.

The tarball also carries the man page and shell completions, which the binary
generates itself:

```console
$ install -Dm644 luthier-*/luthier.1 ~/.local/share/man/man1/luthier.1
$ luthier completions zsh > ~/.zfunc/_luthier
```

From source, with a Rust 1.89 or newer toolchain:

```console
$ cargo build --release
$ install -Dm755 target/release/luthier ~/.local/bin/luthier
```

## Use

```console
luthier refresh                 # fetch registry metadata
luthier search <query>          # find packages
luthier info <package>          # everything known about one
luthier install <package>       # resolve, download, verify, install
luthier list                    # what is installed
luthier list --unmanaged        # plugins installed outside Luthier
luthier verify [package]        # check installed files are unchanged
luthier update [package]        # report updates, or apply named ones
luthier remove <package>        # remove, keeping shared dependencies
luthier cleanup                 # report packages nothing needs
luthier pin <package> [version] # hold a version back from updates
luthier unpin <package>         # let it be updated again

luthier cache list              # what the download cache holds
luthier cache clean             # delete archives nothing installed needs

luthier bench list              # registries, in the order they are consulted
luthier bench add <name> <url|path>
luthier bench remove <name>

luthier completions <shell>     # bash, elvish, fish, powershell or zsh

luthier env create <name>       # a separate set of installed software
luthier env list
luthier env activate <name>     # prints the exports; see below
luthier env deactivate          # prints the exports that undo it
luthier env show                # which environment is in use
luthier env path [name]         # where an environment lives
luthier env remove <name>
luthier env export              # write this installation to a portable file
luthier env import <file>       # rebuild it somewhere else
```

Add `--json` to any read-only command for machine-readable output, `-v` for
detail, `--offline` to work from the cache alone, and `--no-system-plugins` to
resolve as though the machine were bare.

Anything that deletes or installs asks first. In a script, `--yes` answers;
`--json` does not, because choosing how output is rendered is not agreeing to
have files removed. Without a terminal to ask on, such a command refuses rather
than assuming.

Benches are consulted in the order `bench list` prints, and the first to carry
a package ID keeps it — which is how a curated manifest corrects a derived one.
`bench add` appends rather than prepends, so a new bench cannot start
overriding an existing one without being asked to. A bench that has begun
answering from a different host than the one it was first fetched from is
refused rather than believed: the checksums it carries are what every artifact
is verified against.

`--root <dir>` confines what is *written*; the system search paths govern what
is *seen*. They are separate: a rooted install still counts a
distribution-provided engine as satisfying a dependency, unless
`--no-system-plugins` says to ignore it.

A sample library is refused before it is downloaded when nothing on the system
can play it. The library says what it holds (`content = ["sfz"]`), the bench's
`engines.toml` says what plays that, and any one engine — detected, already
installed, or named in the same command — is enough:

```console
$ luthier install crocellkit
warning: nothing on this system can play crocellkit's DrumGizmo content. Any one of these would:
  drumgizmo   Install from your distribution, for example `apt install drumgizmo` …
  drumcraker  luthier install drumcraker
error: not installing crocellkit: nothing on this system can play its DrumGizmo content
```

Until the registry repository is published, point at a local checkout:

```console
$ luthier --registry-path ../luthier-pkgs search synth
```

## Where things go

| What | Where |
|---|---|
| CLAP plugins | `~/.clap/` |
| VST3 plugins | `~/.vst3/` |
| LV2 plugins | `~/.lv2/` |
| Sample libraries, presets, soundfonts | `~/.local/share/luthier/libraries/` |
| State and registry snapshots | `~/.local/share/luthier/` |
| Downloaded artifacts | `~/.cache/luthier/` |
| Configuration | `~/.config/luthier/config.json` |

`--root <dir>` confines all of these to one directory, which is how the test
suite avoids ever touching a real plugin directory.

## Environments

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

Activation exports `LUTHIER_ENV`, which the manager reads, and the three
plugin search-path variables, which hosts read. `--env <name>` does the same
for a single command without touching the shell.

## Moving a setup to another machine

```console
$ luthier env export -o studio.toml       # on the old machine
$ luthier env import studio.toml          # on the new one
```

The file pins the exact version of every package, including the ones that
arrived as dependencies, so the import reproduces the set rather than
installing whatever is newest. `--loose` records names without versions when
that is what you want instead. It records no paths: where things land is the
receiving machine's business.

See [Environments](docs/ENVIRONMENTS.md) for the format.

There is no "current environment" file. A stored pointer would let one terminal
change what another is about to install into, so selection lives in the
environment, as it does for a virtualenv.

One asymmetry is worth knowing, because it comes from the formats rather than
from this tool. `LV2_PATH` *replaces* a host's default search path, so an
environment can fully determine which LV2 bundles a host sees. `CLAP_PATH` and
`VST3_PATH` only *extend* the standard locations, so `~/.clap` and `~/.vst3`
stay visible alongside the environment's own.

## Documentation

- [Roadmap](ROADMAP.md) — what is next, and what is deliberately out of scope
- [Changelog](CHANGELOG.md) — what changed, release by release
- [Architecture](docs/ARCHITECTURE.md) — how the pieces fit together
- [Environments](docs/ENVIRONMENTS.md) — export, import and reproducing a setup
- [Security model](SECURITY.md) — what is trusted, and what is not
- [Manifest format](docs/MANIFEST.md) — the package schema
- [Registry](docs/REGISTRY.md) — adding a package
- [Exit codes](docs/EXIT_CODES.md)
- [Contributing](CONTRIBUTING.md)

## Licence

Copyright (C) 2026 Luthier contributors.

Luthier is free software: you can redistribute it and/or modify it under the
terms of the GNU Lesser General Public License as published by the Free
Software Foundation, either version 2.1 of the License, or (at your option) any
later version. It is distributed in the hope that it will be useful, but
WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE. See the [GNU Lesser General Public
License](LICENSE) for details.

"Or any later version" is load-bearing rather than boilerplate here. Several
dependencies — `spdx`, the 7z and xz decoders, and two crates further down the
tree — are Apache-2.0 only, which the FSF reads as incompatible with the
version-2 licences and compatible with version 3. The option to take this
under LGPL-3.0-or-later is what keeps a build of Luthier distributable.

The package manifests live in a separate repository,
[luthier-pkgs](https://github.com/luthier/luthier-pkgs), under the MIT
licence: the tool stays free, the data stays maximally reusable.

Two things follow for anyone integrating with it.

**Running it was never restricted.** Any program — free or proprietary — may
execute the `luthier` binary and read its `--json` output. Invoking a program
is not derivative work under any of these licences, and it is the intended
integration path for a DAW.

**Linking the library is permitted too, on the LGPL's terms.** A front end may
depend on the `luthier-core` crate and stay under its own licence, provided the
conditions in section 6 of the LGPL are met — chiefly that changes to Luthier
itself remain under the LGPL, that the use is disclosed, and that the user can
replace the Luthier part with a modified version and relink. With Rust's static
linking that last condition is the one that takes work, so a front end that
does not want to deal with it can invoke the binary instead, which carries no
such condition.
