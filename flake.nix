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

      # `nix develop`, or `use flake` through direnv. The package's own inputs
      # (rustc, cargo, and the cmake and perl aws-lc-sys needs) come from
      # `inputsFrom`, so the shell cannot drift from what the build uses.
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.luthier ];
          packages = with pkgs; [
            clippy
            rustfmt
            rust-analyzer
            cargo-deny
            nixfmt
          ];
        };
      });

      checks = forAllSystems (pkgs: {
        luthier = self.packages.${pkgs.stdenv.hostPlatform.system}.luthier;
      });

      # nixfmt-tree, not bare nixfmt: `nix fmt` passes no paths, and nixfmt
      # given none reads stdin and fails. This walks the tree itself.
      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
