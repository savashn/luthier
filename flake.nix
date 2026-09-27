{
  description = "Luthier: a package manager for Linux audio plugins and sample libraries";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # Every artifact the registries carry is for Linux x86_64, so a build
      # for anything else would install nothing.
      systems = [ "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        luthier = pkgs.callPackage ./nix/package.nix { };
        default = luthier;
      });

      overlays.default = final: _prev: {
        luthier = final.callPackage ./nix/package.nix { };
      };

      # `programs.luthier`: the plugins and sample libraries a user wants,
      # declared, and applied on every `home-manager switch`.
      homeManagerModules = {
        luthier = ./nix/hm-module.nix;
        default = self.homeManagerModules.luthier;
      };

      checks = forAllSystems (pkgs: {
        luthier = self.packages.${pkgs.stdenv.hostPlatform.system}.luthier;
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
