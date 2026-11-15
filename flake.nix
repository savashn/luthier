{
  description = "Luthier: a package manager for Linux audio plugins and sample libraries";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # The architectures the registries carry artifacts for: most packages
      # are x86_64-only, but some 65 Open Audio Stack packages publish an
      # aarch64 build too (2026-10).
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
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
      # (rustc, cargo, pkg-config and the AWS-LC aws-lc-sys links) come from
      # `inputsFrom`, so the shell cannot drift from what the build uses.
      # AWS_LC_SYS_USE_SYSTEM is set as the build's hook sets it, so a shell
      # that cannot find AWS-LC fails instead of quietly compiling the
      # vendored copy.
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.luthier ];
          env.AWS_LC_SYS_USE_SYSTEM = "1";
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
