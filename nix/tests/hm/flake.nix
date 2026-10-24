# programs.luthier, built by real Home Manager, for run.sh to try in a
# scratch HOME. Its own flake, so Home Manager never becomes an input of the
# flake users consume.
{
  description = "Home Manager configurations that exercise programs.luthier";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # The checkout this flake sits in, uncommitted changes included.
    luthier = {
      url = "path:../../..";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      home-manager,
      luthier,
      ...
    }:
    let
      pkgs = nixpkgs.legacyPackages.x86_64-linux;
      # Every path the generated scripts name is fixed when they are built,
      # so run.sh uses this same directory.
      scratch = "/tmp/luthier-hm-test";
      home =
        extra:
        home-manager.lib.homeManagerConfiguration {
          inherit pkgs;
          modules = [
            luthier.homeManagerModules.default
            {
              # Never activated, so the name is never checked.
              home.username = "luthier-test";
              home.homeDirectory = "${scratch}/home";
              home.stateVersion = "25.05";
              # Not the default, so a service that ignores it is caught.
              xdg.enable = true;
              xdg.dataHome = "${scratch}/home/data";
              programs.luthier = {
                enable = true;
                # 94 KB, from extras.
                packages = [ "matsuri" ];
              };
            }
            extra
          ];
        };
    in
    {
      homeConfigurations = {
        background = home { };
        missing-location = home {
          programs.luthier.locations.libraries = "${scratch}/samples";
        };
        activation = home {
          programs.luthier.apply = "activation";
          programs.luthier.locations.libraries = "${scratch}/samples-activation";
        };
        no-xdg = home { xdg.enable = nixpkgs.lib.mkForce false; };
      };
    };
}
