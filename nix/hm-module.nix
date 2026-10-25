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
#   update     `luthier update` first, so a newly declared package can be
#              found
#   apply      where those run: a systemd user service the switch starts
#              and does not wait for (`background`), or the activation
#              script itself (`activation`)
#   notify     a desktop notification when the service finishes
#
# A failure — no network, a disk that is not mounted — is reported as a
# warning and does not fail the switch: the rest of a Home Manager
# configuration should not depend on a plugin registry being reachable.
#
# Applying in the background exists because installing sample libraries can
# take an hour, and the NixOS module gives activation five minutes before it
# kills it, and the rest of the switch with it. The switch records which
# declaration it wants (`wanted`, a link to the script that applies it); the
# service applies it and records it as done (`applied`) only once every
# step succeeded. So it runs when the declaration changed or the last run
# did not finish, one at a time, since systemd starts a unit only once; a
# switch while it runs leaves it to finish, and it then applies the newest
# declaration before it stops.
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
            `luthier upgrade`.
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

  # luthier finds its state, config and cache through the XDG variables,
  # and neither the user manager nor a switch run by the NixOS module need
  # have the ones a login shell does. Where Home Manager manages them, the
  # values it puts in the session are passed on, so the declaration is
  # applied to the installation an interactive `luthier` reads.
  exe = toString (
    lib.optionals config.xdg.enable [
      "${pkgs.coreutils}/bin/env"
      "XDG_CONFIG_HOME=${escapeShellArg config.xdg.configHome}"
      "XDG_DATA_HOME=${escapeShellArg config.xdg.dataHome}"
      "XDG_CACHE_HOME=${escapeShellArg config.xdg.cacheHome}"
    ]
    ++ [ (lib.getExe cfg.package) ]
  );
  globalFlags = [ "--yes" ] ++ lib.optional (!cfg.systemPlugins) "--no-system-plugins";
  pruneFlag = optionalString cfg.prune " --prune";

  # Applying an empty list only makes sense when it is meant to remove
  # everything; without prune it would ask for nothing and change nothing.
  applies = cfg.packages != [ ] || cfg.prune;

  pluginsDir = cfg.locations.plugins;

  # The steps, each through a `luthier` function the caller defines, which
  # returns its status: the activation script warns and goes on, the
  # service also remembers that something failed. `--keep-going`, so one
  # package that cannot be installed does not keep every one after it from
  # ever being installed.
  #
  # Nothing is imported while a location could not be set: the disk it
  # names may not be mounted yet, and the import would install into the
  # directory used before — the home disk, on a first run — which is what
  # refusing to create a location exists to prevent. Every step is part of
  # an `||` or an `if`, since activation runs under `set -e`.
  steps = ''
    ${optionalString cfg.update "luthier update || :"}

    locations_set=1
    ${concatMapStrings
      (kind: ''
        luthier location set ${kind} ${escapeShellArg cfg.locations.${kind}} || locations_set=0
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
      if [ "$locations_set" = 1 ]; then
        luthier import --keep-going ${declared}${pruneFlag} || :
      else
        echo "luthier: a location could not be set, so nothing is imported" >&2
      fi
    ''}
  '';

  # One declaration, applied: its store path is what `wanted` and `applied`
  # name, so a change to anything it runs — the packages, a flag, luthier
  # itself — is a declaration not yet applied.
  applyScript = pkgs.writeShellScript "luthier-apply" ''
    failed=0
    luthier() {
      ${exe} ${lib.escapeShellArgs globalFlags} "$@" || {
        echo "luthier $*: failed" >&2
        failed=1
        return 1
      }
    }

    ${steps}

    exit "$failed"
  '';

  # Fixed when the configuration is built, not read from $XDG_STATE_HOME:
  # activation writes here and the user service reads, and the two need not
  # share an environment.
  stateDir = escapeShellArg "${config.xdg.stateHome}/luthier/home-manager";

  notifySend =
    urgency: message:
    optionalString cfg.notify ''
      ${pkgs.libnotify}/bin/notify-send --app-name=Luthier --urgency=${urgency} \
        Luthier ${escapeShellArg message} || true
    '';

  # A run that fails is started again this long after, this many times,
  # before it waits for the next login or switch: a network not up yet at
  # login fails one, and a download that keeps failing its checksum should
  # not be fetched again every quarter of an hour.
  retryMinutes = 15;
  retries = 3;
  # The runner's status that asks systemd for another try (EX_TEMPFAIL);
  # 1 is final.
  retryStatus = 75;

  # A run is announced once it has taken this long, so one with nothing to
  # download says nothing, when it starts or when it is done.
  announceAfterSeconds = 60;

  # Applies whatever the last switch declared, until nothing newer is.
  runner = pkgs.writeShellScript "luthier-home-manager" ''
    PATH=${lib.makeBinPath [ pkgs.coreutils ]}
    state=${stateDir}

    # Left by the announcement, so the run knows to say how it ended.
    rm -f "$state/announced"
    ${optionalString cfg.notify ''
      (
        trap 'kill "$sleeper" 2>/dev/null; exit' TERM
        sleep ${toString announceAfterSeconds} &
        sleeper=$!
        wait "$sleeper"
        : > "$state/announced"
        ${notifySend "normal" "Installing the declared packages in the background. You will be told when it is done; journalctl --user -u luthier -f follows it."}
      ) &
      announcer=$!
      trap 'kill "$announcer" 2>/dev/null; rm -f "$state/announced"' EXIT
    ''}
    announced() {
      [ -e "$state/announced" ]
    }

    # How many tries of the declaration `wanted` names have failed since the
    # last login or switch: its first line names the declaration, so one a
    # switch replaced starts again from none.
    failures() {
      if [ "$(sed -n 1p "$state/attempts" 2>/dev/null)" = "$wanted" ]; then
        sed -n 2p "$state/attempts"
      else
        echo 0
      fi
    }

    while :; do
      wanted=$(readlink "$state/wanted") || exit 0
      if [ "$wanted" = "$(cat "$state/applied" 2>/dev/null)" ]; then
        exit 0
      fi
      # The generation that declared it was collected: a switch since has
      # declared something else, or nothing.
      if [ ! -x "$wanted" ]; then
        echo "$wanted is gone; switch again to apply the declaration" >&2
        exit 1
      fi

      # Not applied while it runs: a run cut short, or one that failed after
      # a switch back to what was applied before, has changed the
      # installation since, so that declaration must run again.
      rm -f "$state/applied"
      echo "Applying $wanted"
      if "$wanted"; then
        printf '%s\n' "$wanted" > "$state/applied.new"
        mv "$state/applied.new" "$state/applied"
        rm -f "$state/attempts"
        if [ "$(readlink "$state/wanted")" = "$wanted" ]; then
          echo "The declared packages are installed."
          if announced; then
            ${notifySend "normal" "The declared packages are installed."}
            :
          fi
          exit 0
        fi
      elif [ "$(readlink "$state/wanted")" = "$wanted" ]; then
        tries=$(($(failures) + 1))
        if [ "$tries" -le ${toString retries} ]; then
          printf '%s\n%s\n' "$wanted" "$tries" > "$state/attempts"
          echo "Applying failed; trying again in ${toString retryMinutes} minutes ($tries of ${toString retries})" >&2
          if announced; then
            ${notifySend "normal" "Some packages could not be installed yet; trying again in ${toString retryMinutes} minutes."}
            :
          fi
          exit ${toString retryStatus}
        fi
        rm -f "$state/attempts"
        echo "Applying failed again after ${toString retries} retries; trying again at the next login or switch" >&2
        ${notifySend "critical" "Installing the declared packages failed. journalctl --user -u luthier says why."}
        exit 1
      fi
      # A switch declared something newer meanwhile.
    done
  '';

  # A login starts it afresh, with every retry to come.
  resume = pkgs.writeShellScript "luthier-resume" ''
    ${pkgs.coreutils}/bin/rm -f ${stateDir}/attempts
    exec ${config.systemd.user.systemctlPath} --user start --no-block luthier.service
  '';

  background = cfg.apply == "background";
  systemctl = ''env XDG_RUNTIME_DIR="''${XDG_RUNTIME_DIR:-/run/user/$(id -u)}" ${config.systemd.user.systemctlPath}'';
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

    update = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Run `luthier update` before applying, so a newly declared package can
        be found. It asks for the package list each time the declaration is
        applied, which downloads it only when it changed. Off, the list is
        asked for only once a day has passed, as it is by any command.
      '';
    };

    apply = mkOption {
      type = types.enum [
        "background"
        "activation"
      ];
      default = "background";
      description = ''
        Where the declaration is applied.

        `background`
        : In `luthier.service`, a systemd user service the switch starts
          and does not wait for. It runs only when the declaration changed
          or the last run did not finish, one at a time, and after a login
          if a run was cut short. A run that fails is tried again a quarter
          of an hour later, three times. Follow it with
          `journalctl --user -u luthier -f`; `systemctl --user start luthier`
          waits for it.

        `activation`
        : During the switch, which waits for every download. The NixOS
          module stops activation after five minutes, so this suits a
          declaration that installs little at a time, or a system without a
          systemd user session.
      '';
    };

    notify = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Desktop notifications from `luthier.service`: once a run has taken a
        minute, again when it is done, and whenever it gives up. A run with
        nothing to download says nothing. Without a notification daemon
        nothing is shown. Only with `apply = "background"`.
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

    home.activation =
      if background then
        {
          # Before sd-switch starts anything, so a service it starts already
          # reads this declaration rather than the last one.
          luthierDeclare = lib.hm.dag.entryBetween [ "reloadSystemd" ] [ "writeBoundary" ] ''
            run mkdir -p ${stateDir}
            run ln -sfn ${applyScript} ${stateDir}/wanted
          '';

          luthier = lib.hm.dag.entryAfter [ "reloadSystemd" ] ''
            if [ "$(readlink ${stateDir}/wanted)" = "$(cat ${stateDir}/applied 2>/dev/null)" ]; then
              verboseEcho "luthier: the declaration is already applied"
            # A switch starts it afresh, with every retry to come, and at
            # once if it was waiting for one.
            elif run rm -f ${stateDir}/attempts \
              && run ${systemctl} --user start --no-block luthier.service; then
              echo "luthier: applying the declaration in the background;" \
                "follow it with journalctl --user -u luthier -f"
            else
              warnEcho "luthier: could not start luthier.service; it applies the declaration at the next login"
            fi
          '';
        }
      else
        {
          luthier = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
            luthier() {
              run ${exe} ${lib.escapeShellArgs globalFlags} "$@" || {
                warnEcho "luthier $*: failed; the rest of the switch goes ahead"
                return 1
              }
            }

            ${steps}
          '';
        };

    systemd.user.services = mkIf background {
      luthier = {
        Unit = {
          Description = "Install the packages programs.luthier declares";
          # A switch never stops a run halfway: the run picks up what the
          # switch declared once it is done.
          X-SwitchMethod = "keep-old";
        };
        Service = {
          Type = "oneshot";
          ExecStart = "${runner}";
          # Downloading and unpacking gigabytes is not what the desktop
          # should wait behind.
          Nice = 10;
          # The runner asks for another try with its own status, and ends
          # with 1 once the tries are spent.
          Restart = "on-failure";
          RestartSec = "${toString retryMinutes}min";
          RestartPreventExitStatus = 1;
        };
      };

      # Starts it after a login, in case a run was cut short or the switch
      # could not reach the user manager. Not luthier.service itself wanted
      # by default.target: the login, and sd-switch on the first switch,
      # would wait for an hour of downloads.
      luthier-resume = {
        Unit.Description = "Finish installing what programs.luthier declares";
        Service = {
          Type = "oneshot";
          ExecStart = "${resume}";
        };
        Install.WantedBy = [ "default.target" ];
      };
    };
  };
}
