{
  description = "Cbar GTK4 desktop panel";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-compat.url = "github:edolstra/flake-compat";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      ...
    }:
    let
      forAllSystems =
        function:
        nixpkgs.lib.genAttrs nixpkgs.lib.systems.flakeExposed (
          system:
          function (import nixpkgs {
            inherit system;
            # FSL is source-available but not OSI-free. Keep the exception
            # scoped to cbar so consumers can build this flake without a
            # global unfree policy.
            config.allowUnfreePredicate =
              pkg: nixpkgs.lib.hasPrefix "cbar" (nixpkgs.lib.getName pkg);
          })
        );
    in
    {
      # Devshell
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            clippy
            rustfmt
            rust-analyzer
            sccache
            dbus
          ];

          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.cbar ];
        };

      });

      # Packages
      packages = forAllSystems (pkgs: {
        cbar =
          let
            props = builtins.fromTOML (builtins.readFile ./Cargo.toml);
            version = props.package.version;
            craneLib = crane.mkLib pkgs;
          in
          pkgs.callPackage ./nix/package.nix {
            inherit version craneLib;
          };

        default = self.packages.${pkgs.stdenv.hostPlatform.system}.cbar;
      });

      # Apps
      apps = forAllSystems (
        pkgs:
        let
          cbar = {
            type = "app";
            program = pkgs.lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.cbar;
          };
        in
        {
          inherit cbar;
          default = cbar;
        }
      );

      homeManagerModules.default = import ./nix/module.nix self;
    };

}
