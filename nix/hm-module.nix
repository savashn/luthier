# programs.luthier: plugins and sample libraries, declared.
#
# Everything here is applied by running luthier itself during activation,
# rather than by building the plugins as derivations: the manager already
# verifies every download against its manifest's checksum, never runs
# anything from a package, and never writes outside its own directories, and
# doing the same work twice in two languages is how the two drift apart.
#
# What each option turns into:
#
#   packages   a file in the format `luthier export` writes, applied with
#              `luthier import [--prune]`
#   locations  `luthier location set`
#   refresh    `luthier refresh` first, so a newly declared package can be
#              found
#
# A failure — no network, a disk that is not mounted — is reported as a
# warning and does not fail the switch: the rest of a Home Manager
# configuration should not depend on a plugin registry being reachable.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  inherit (lib)
    concatMapStrings
    escapeShellArg
    literalExpression
    mkEnableOption
    mkIf
    mkOption
    optionalAttrs
    optionalString
    types
    ;

  cfg = config.programs.luthier;
  toml = pkgs.formats.toml { };

  packageType = types.coercedTo types.str (id: { inherit id; }) (
    types.submodule {
      options = {
        id = mkOption {
          type = types.str;
          example = "surge";
          description = "The package ID, as `luthier search` shows it.";
        };
        version = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "1.3.4";
          description = ''
            Hold the package at exactly this version. Applying fails while
            the registry no longer offers it. `null` follows the registry:
            installed at the newest version, and upgraded with
            `luthier update`.
          '';
        };
      };
    }
  );

  packagesOption = mkOption {
    type = types.listOf packageType;
    default = [ ];
    example = literalExpression ''
      [
        "surge"
        "lsp-plugins"
        { id = "dragonfly-reverb"; version = "3.2.10"; }
      ]
    '';
    description = ''
      Packages to install: plugins into `~/.clap`, `~/.vst3` and `~/.lv2`,
      sample libraries into the library directory. A plain string is a
      package ID; an attribute set can also hold it at a version.
      Dependencies need not be listed.
    '';
  };

  # The same format `luthier export --loose` writes. A version is a hard
  # requirement where one is given; the timestamp is fixed so the file, and
  # therefore the derivation, only changes when the declaration does.
  declared = toml.generate "luthier-packages.toml" {
    meta = {
      schema = 1;
      luthier = cfg.package.version;
      exported = "1970-01-01T00:00:00Z";
      pinned = false;
    };
    package = map (
      p:
      {
        inherit (p) id;
        reason = "explicit";
      }
      // optionalAttrs (p.version != null) { inherit (p) version; }
    ) cfg.packages;
  };

  exe = lib.getExe cfg.package;
  globalFlags = [ "--yes" ] ++ lib.optional (!cfg.systemPlugins) "--no-system-plugins";
  pruneFlag = optionalString cfg.prune " --prune";

  # Applying an empty list only makes sense when it is meant to remove
  # everything; without prune it would ask for nothing and change nothing.
  applies = cfg.packages != [ ] || cfg.prune;

  pluginsDir = cfg.locations.plugins;
in
{
  options.programs.luthier = {
    enable = mkEnableOption "Luthier, a package manager for Linux audio plugins and sample libraries";

    package = mkOption {
      type = types.package;
      default = pkgs.callPackage ./package.nix { };
      defaultText = literalExpression "luthier built from this flake";
      description = "The luthier package to install and to apply the declaration with.";
    };

    packages = packagesOption;

    prune = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Remove installed packages the declaration does not list, so the
        installation ends up exactly as declared. Off by default, so enabling the module never removes
        something installed by hand; turn it on once the declaration lists
        everything you want to keep. Files changed since they were installed
        are kept either way.
      '';
    };

    refresh = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Run `luthier refresh` before applying, so a newly declared package can
        be found. It fetches the package lists on every switch; turn it off to
        apply from the lists already on disk.
      '';
    };

    systemPlugins = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Count plugins installed outside Luthier — by nixpkgs, or by your
        distribution — when deciding whether a sample library has something to
        play it. Off is `--no-system-plugins`.
      '';
    };

    locations =
      let
        location =
          what:
          mkOption {
            type = types.nullOr types.str;
            default = null;
            example = "/mnt/samples";
            description = ''
              Keep ${what} in this directory instead of the default, for
              instance on another disk. It must already exist: Luthier never
              creates it, so an unmounted disk is noticed instead of filled
              underneath. `null` leaves the setting as it is. Moving a
              location that packages are installed in is refused until they
              are removed.
            '';
          };
      in
      {
        cache = location "downloaded archives";
        libraries = location "sample libraries";
        plugins = location "plugins, in `clap`, `vst3` and `lv2` subdirectories,";
      };

    setSearchPath = mkOption {
      type = types.bool;
      default = true;
      description = ''
        With `locations.plugins` set, export `CLAP_PATH`, `VST3_PATH` and
        `LV2_PATH` so hosts find the plugins there, as
        `luthier location search-path` would.
      '';
    };
  };

  config = mkIf cfg.enable {
    home.packages = [ cfg.package ];

    # CLAP_PATH and VST3_PATH add to a host's own search path, so the
    # directory is simply put in front. LV2_PATH replaces it, so the places
    # a host would otherwise have looked are listed again — the user's
    # ~/.lv2, the Nix profiles and the conventional system directories.
    home.sessionVariables = mkIf (pluginsDir != null && cfg.setSearchPath) {
      CLAP_PATH = "${pluginsDir}/clap\${CLAP_PATH:+:$CLAP_PATH}";
      VST3_PATH = "${pluginsDir}/vst3\${VST3_PATH:+:$VST3_PATH}";
      LV2_PATH = lib.concatStringsSep ":" [
        "${pluginsDir}/lv2"
        "\${LV2_PATH:-$HOME/.lv2:$HOME/.nix-profile/lib/lv2:/etc/profiles/per-user/$USER/lib/lv2:/run/current-system/sw/lib/lv2:/usr/lib/lv2:/usr/local/lib/lv2}"
      ];
    };

    home.activation.luthier = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
      luthier() {
        run ${exe} ${lib.escapeShellArgs globalFlags} "$@" \
          || warnEcho "luthier $*: failed; the rest of the switch goes ahead"
      }

      ${optionalString cfg.refresh "luthier refresh"}

      ${concatMapStrings
        (kind: ''
          luthier location set ${kind} ${escapeShellArg cfg.locations.${kind}}
        '')
        (
          builtins.filter (kind: cfg.locations.${kind} != null) [
            "cache"
            "libraries"
            "plugins"
          ]
        )
      }

      ${optionalString applies ''
        luthier import ${declared}${pruneFlag}
      ''}
    '';
  };
}
