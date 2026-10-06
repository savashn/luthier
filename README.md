# Luthier

A DAW-agnostic command-line package manager for Linux audio software.
Provides CLAP, VST3 and LV2 plugins and sample libraries.

<a href="https://github.com/open-audio-stack"><img src="https://raw.githubusercontent.com/open-audio-stack/open-audio-stack-registry/refs/heads/main/src/assets/powered-by-open-audio-stack.svg" alt="Powered by Open Audio Stack"></a>

- [Status](#status)
- [Install](#install)
  - [Debian and Ubuntu](#debian-and-ubuntu)
  - [Fedora and openSUSE](#fedora-and-opensuse)
  - [Install script](#install-script)
  - [By hand](#by-hand)
  - [Checking a download](#checking-a-download)
  - [With Nix](#with-nix)
  - [First steps](#first-steps)
- [Use](#use)
- [Where things go](#where-things-go)
- [Moving a setup to another machine](#moving-a-setup-to-another-machine)
- [Documentation](#documentation)
- [Licence](#licence)

Linux audio plugins are scattered across GitHub releases, GitLab, vendor sites
and distribution repositories, in a handful of archive formats and several
plugin formats. Installing one usually means finding the project, finding the
release, downloading an archive, working out which directory each format
belongs in, and copying files by hand.

Luthier replaces that with:

```console
$ luthier search synth
ID           NAME              VERSION  CATEGORY    TAGS
dexed        Dexed             1.0.1    instrument  synthesizer, fm
surge        Surge XT          1.3.4    instrument  synthesizer

$ luthier install surge
$ luthier list
$ luthier remove surge
```

## Install

Luthier runs on Linux x86_64. Every way below installs the same statically
linked binary; there is no runtime to install and no toolchain to build it
with. The packages and the install script also put its man page and shell
completions in place. Pick the line for your system:

| Your system | Install with |
|---|---|
| Debian, Ubuntu, Ubuntu Studio, Linux Mint | [the .deb](#debian-and-ubuntu) |
| Fedora, Fedora Jam, openSUSE | [the .rpm](#fedora-and-opensuse) |
| NixOS, or Nix on any distribution | [the flake](#with-nix) |
| Anything else, or without root | [the install script](#install-script), or [by hand](#by-hand) |

A package or script from a release does not update itself: when a new release
comes out, install it the same way.

### Debian and Ubuntu

```console
curl -LO https://github.com/savashn/luthier/releases/latest/download/luthier-x86_64-linux.deb
sudo apt install ./luthier-x86_64-linux.deb
```

Opening the downloaded file in your software centre works too.
`sudo apt remove luthier` removes it.

### Fedora and openSUSE

```console
sudo dnf install https://github.com/savashn/luthier/releases/latest/download/luthier-x86_64-linux.rpm
```

On openSUSE, download it and install it with zypper. The package is not
signed with a key, so zypper has to be told to accept that; what proves where
it came from is the [attestation](#checking-a-download):

```console
curl -LO https://github.com/savashn/luthier/releases/latest/download/luthier-x86_64-linux.rpm
sudo zypper install --allow-unsigned-rpm ./luthier-x86_64-linux.rpm
```

`sudo dnf remove luthier` or `sudo zypper remove luthier` removes it.

### Install script

Installs into `~/.local`, without root:

```console
curl -fsSL https://github.com/savashn/luthier/releases/latest/download/install.sh | sh
```

The script downloads the release's tarball and refuses it unless its SHA-256
is the one the release wrote into the script. To read it before it runs,
download it and run it with `sh install.sh`. `LUTHIER_PREFIX=/some/dir` installs
somewhere else; `sh install.sh --uninstall` removes what it installed.

### By hand

The tarball the other ways are made from:

```console
curl -LO https://github.com/savashn/luthier/releases/latest/download/luthier-x86_64-linux.tar.gz
tar xzf luthier-x86_64-linux.tar.gz
install -Dm755 luthier-*/luthier ~/.local/bin/luthier
```

Nothing needs root, and nothing else needs installing.

If `luthier --version` then says the command is not found, `~/.local/bin` is
not on your `PATH`. Add it once, to `~/.bashrc` or `~/.zshrc` depending on
your shell, and open a new terminal:

```console
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
```

The tarball also carries the man page and shell completions, which the binary
generates itself:

```console
install -Dm644 luthier-*/luthier.1 ~/.local/share/man/man1/luthier.1
mkdir -p ~/.zfunc && luthier completions zsh > ~/.zfunc/_luthier
```

From source, with a Rust 1.93 or newer toolchain:

```console
cargo build --release
install -Dm755 target/release/luthier ~/.local/bin/luthier
```

### Checking a download

Compare `sha256sum` of the file with the SHA-256 GitHub shows beside it on the
[release page](https://github.com/savashn/luthier/releases/latest). With the
[GitHub CLI](https://cli.github.com), one command checks both that and that the
file was built by this repository's release workflow, from which commit; it
works for the tarball, the .deb, the .rpm and the install script alike:

```console
gh attestation verify luthier-x86_64-linux.tar.gz -R savashn/luthier
```

### With Nix

The repository is a flake. To try it once, or to install it into your
profile:

```console
nix run github:savashn/luthier -- search reverb
nix profile install github:savashn/luthier
```

The flake also has an overlay for a NixOS configuration, and a Home Manager
module, `programs.luthier`, that declares your plugins and sample libraries
and installs them on every `home-manager switch`. Adding the flake, every
option of the module and what a switch does are in
[Nix and Home Manager](docs/NIX.md).

### First steps

Fetch the package lists first; nothing can be found or installed until this
has run once:

```console
luthier refresh
luthier search reverb
luthier install dragonfly-reverb
luthier list
```

Plugins land in `~/.clap`, `~/.vst3` and `~/.lv2`, which hosts already scan:
restart your DAW, or ask it to rescan, and they appear. Anything that installs
or deletes shows what it will do and asks first. Run `luthier refresh` again
now and then to see new packages and versions, and `luthier update` to see
what can be upgraded.

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

luthier bench list              # where packages come from, in precedence order

luthier completions <shell>     # bash, elvish, fish, powershell or zsh

luthier export                  # write this installation to a portable file
luthier import <file>           # rebuild it somewhere else
```

Add `--json` to any read-only command for machine-readable output, `-v` for
detail, `--offline` to work from the cache alone, and `--no-system-plugins` to
resolve as though the machine were bare.

Anything that deletes or installs asks first. In a script, `--yes` answers;
`--json` does not, because choosing how output is rendered is not agreeing to
have files removed. Without a terminal to ask on, such a command refuses rather
than assuming.

The bench is consulted before the Open Audio Stack registry, and the first to
carry a package ID keeps it — which is how a curated manifest corrects a
derived one.

`--root <dir>` confines what is *written*; the system search paths govern what
is *seen*. They are separate: a rooted install still counts a
distribution-provided engine as satisfying a dependency, unless
`--no-system-plugins` says to ignore it.

A sample library says so before it is downloaded when nothing on the system
can play it, and names what would. The library says what it holds
(`content = ["sfz"]`), a built-in list and any bench's `engines.toml` say what
plays that, and any one engine — detected, already installed, or named in the
same command — is enough. The warning comes with the plan, so you decide
whether to go ahead:

```console
$ luthier install crocellkit
warning: crocellkit holds DrumGizmo content, and playing it needs DrumGizmo, or another player of its kits. Nothing on this system looks like one; any of these would do:
  drumgizmo   Install from your distribution, for example `apt install drumgizmo` …
  drumcraker  luthier install drumcraker
```

## Where things go

| What | Where |
| --- | --- |
| CLAP plugins | `~/.clap/` |
| VST3 plugins | `~/.vst3/` |
| LV2 plugins | `~/.lv2/` |
| Sample libraries, presets, soundfonts | `~/.local/share/luthier/libraries/` |
| State and registry snapshots | `~/.local/share/luthier/` |
| Downloaded artifacts | `~/.cache/luthier/` |
| Configuration | `~/.config/luthier/config.json` |

`--root <dir>` confines all of these to one directory, which is how the test
suite avoids ever touching a real plugin directory.

## Moving a setup to another machine

```console
luthier export -o studio.toml       # on the old machine
luthier import studio.toml          # on the new one
```

The file pins the exact version of every package, including the ones that
arrived as dependencies, so the import reproduces the set rather than
installing whatever is newest. `--loose` records names without versions when
that is what you want instead. It records no paths: where things land is the
receiving machine's business. `luthier import --prune` also removes whatever
the file does not list, so the installation ends up exactly as described.

See [Export and import](docs/EXPORT.md) for the format.

## Documentation

- [Roadmap](ROADMAP.md) — what is next, and what is deliberately out of scope
- [Changelog](CHANGELOG.md) — what changed, release by release
- [Architecture](docs/ARCHITECTURE.md) — how the pieces fit together
- [Export and import](docs/EXPORT.md) — reproducing a setup on another machine
- [Nix and Home Manager](docs/NIX.md) — `programs.luthier`, and the flake
- [Security model](SECURITY.md) — what is trusted, and what is not
- [Scope](docs/SCOPE.md) — what it will not do, and why some plugins are missing
- [DAWs installed from Flathub](docs/FLATHUB.md) — what a sandboxed host sees
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

The package manifests live in `bench/`, under the MIT licence rather than the
LGPL that covers the rest of the repository: the tool stays free, the data
stays maximally reusable. See `bench/LICENSE`.

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
