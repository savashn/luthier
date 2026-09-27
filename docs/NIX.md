# Nix and Home Manager

Luthier ships a flake with a package and a Home Manager module,
`programs.luthier`. With the module, the plugins and sample libraries you use
are declared in your Home Manager configuration and installed on every
`home-manager switch`, alongside the rest of your setup.

nixpkgs already packages many plugins and the engines that play sample
content — sfizz, DrumGizmo, LSP Plugins. What it does not carry is the content
itself: drum kits, SFZ and SoundFont libraries, measured in gigabytes. Nor does
Home Manager have an option for the set of plugins a DAW should see. That is
what this module is for. It works alongside plugins from nixpkgs: Luthier looks
in your Nix profiles when it decides whether a sample library has something to
play it.

## Contents

- [Adding the flake](#adding-the-flake)
- [A first configuration](#a-first-configuration)
- [Options](#options)
- [What a switch does](#what-a-switch-does)
- [Another disk](#another-disk)
- [Without Home Manager](#without-home-manager)

## Adding the flake

In the flake that holds your Home Manager configuration:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    luthier = {
      url = "github:savashn/luthier";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, home-manager, luthier, ... }: {
    homeConfigurations."you" = home-manager.lib.homeManagerConfiguration {
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
      modules = [
        luthier.homeManagerModules.default
        ./home.nix
      ];
    };
  };
}
```

With Home Manager as a NixOS module instead, add the module to
`home-manager.sharedModules`, or import it in your user's configuration:

```nix
home-manager.users.you = {
  imports = [ luthier.homeManagerModules.default ];
};
```

To follow a release rather than `main`, pin the input to a tag:
`url = "github:savashn/luthier/v0.2.0"`.

## A first configuration

In `home.nix`:

```nix
{
  programs.luthier = {
    enable = true;
    packages = [
      "surge"
      "lsp-plugins"
      "dragonfly-reverb"
    ];
  };
}
```

Run `home-manager switch`. Luthier fetches the package lists, installs the
three packages into `~/.clap`, `~/.vst3` and `~/.lv2`, and the `luthier`
command is on your `PATH` with its man page and shell completions. Restart
your DAW, or ask it to rescan, and the plugins appear.

Find package IDs with `luthier search <word>` and `luthier info <id>`.

Adding a package to the list and switching again installs it. Removing one
from the list does **not** uninstall it until you turn on `prune`; see below.

## Options

| Option | Default | What it does |
|---|---|---|
| `enable` | `false` | Install `luthier` and apply the declaration on every switch. |
| `package` | built from the flake | The luthier package to use. |
| `packages` | `[ ]` | Packages to install in your home directory. |
| `prune` | `false` | Remove installed packages the declaration does not list. |
| `refresh` | `true` | Fetch the package lists before applying. |
| `systemPlugins` | `true` | Count plugins from nixpkgs or your distribution as engines. |
| `locations.cache` | `null` | Where downloaded archives go. |
| `locations.libraries` | `null` | Where sample libraries go. |
| `locations.plugins` | `null` | Where plugins go. |
| `setSearchPath` | `true` | With `locations.plugins` set, tell hosts where to look. |

### `packages`

Each entry is a package ID, or an attribute set that can also hold the
package at a version:

```nix
programs.luthier.packages = [
  "surge"                                         # newest version
  { id = "lsp-plugins"; version = "1.2.35"; }     # exactly this version
];
```

A package without a version is installed at the newest version the registry
offers, and later upgraded with `luthier update`; a switch does not upgrade
it. A package with a version is held there: if you change the version,
the next switch installs it. If the registry no longer offers that version,
applying fails with a warning. List only what you want; dependencies come with
it.

### `prune`

Off, a switch only adds: a package you remove from the list stays installed,
and so does anything you installed by hand with `luthier install`. That makes
enabling the module safe on a machine that already has plugins.

On, a switch makes the installation match the declaration exactly: every
package the declaration neither lists nor needs is removed, including any
you installed by hand.

```nix
programs.luthier = {
  enable = true;
  prune = true;
  packages = [ "surge" ];   # everything else Luthier installed is removed
};
```

With `prune` on and `packages = [ ]`, a switch removes every package from your
home directory. Removal never deletes a file you changed after it was
installed; Luthier reports it and leaves it where it is.

### `refresh`

On, every switch fetches the package lists first, so a package declared for
the first time can be found. Off, the switch works from the lists already on
disk, which is faster and works offline; run `luthier refresh` yourself now
and then.

### `systemPlugins`

A sample library needs something to play it — an SFZ kit needs sfizz or
another SFZ player. Luthier looks for one in `~/.clap`, `~/.vst3` and `~/.lv2`,
in your Nix profiles (`~/.nix-profile`, `/etc/profiles/per-user/<you>`,
`/run/current-system/sw`) and in the usual system directories. If none is
found it warns before installing, and names what would play it. Turn this off
to ignore everything Luthier did not install itself.

So an engine can come from nixpkgs while the content comes from Luthier:

```nix
home.packages = [ pkgs.sfizz ];                           # the player
programs.luthier.packages = [ "salamander-drumkit" ];     # the samples
```

## What a switch does

During activation, after Home Manager has written your files, the module runs
`luthier` with `--yes`:

1. `luthier refresh`, unless `refresh = false`.
2. `luthier location set <kind> <dir>` for each location that is set.
3. `luthier import <file>`, with `--prune` if `prune` is on. The file is
   generated from `packages`, in the same format `luthier export` writes.

If a step fails — no network, a registry that is down, a disk that is not
mounted — Home Manager prints a warning and the rest of the switch goes ahead.
Nothing else in your configuration depends on a plugin registry being
reachable. Run `home-manager switch` again once the problem is gone.

`home-manager switch --dry-run` prints the commands instead of running them.

Everything is installed in your home directory; nothing needs root, and
nothing is added to the Nix store except `luthier` itself and the generated
files.

## Another disk

Sample libraries in particular can be large enough to belong on another disk:

```nix
programs.luthier.locations = {
  libraries = "/mnt/audio/libraries";
  plugins   = "/mnt/audio/plugins";
};
```

The directories must already exist. Luthier never creates them, so that a
disk that is not mounted is noticed rather than filled underneath: until it
is mounted, a switch warns and installs nothing there. Moving a location that
packages are already installed in is refused; remove them, switch, and they
are installed in the new place.

With `locations.plugins` set, hosts would not find the plugins in their new
home, so the module sets `CLAP_PATH`, `VST3_PATH` and `LV2_PATH` for your
session (`setSearchPath`). Log out and back in, or start a new login shell,
for hosts to see them.

`null`, the default, leaves a location as it is — including one you set by
hand with `luthier location set`.

## Without Home Manager

The package on its own:

```console
$ nix run github:savashn/luthier -- search reverb
$ nix profile install github:savashn/luthier
```

Or in a NixOS configuration, through the overlay:

```nix
nixpkgs.overlays = [ luthier.overlays.default ];
environment.systemPackages = [ pkgs.luthier ];
```

The declarative part is two commands the module runs for you, and they work
the same by hand. Write the file once, keep it in version control, and apply
it on any machine:

```console
$ luthier export --loose -o plugins.toml
$ luthier import --prune plugins.toml
```
