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
          # RUNPATH of everything linked in this shell: a binary has to start
          # without LD_LIBRARY_PATH (env -i, a child with a cleared
          # environment), and it must not name a directory that does not
          # exist.
          #
          # A shell has no $out: `nix develop` points it at ./outputs/out, a
          # directory that is never built. stdenv would still put
          # `-rpath $out/lib` into NIX_LDFLAGS, so NIX_NO_SELF_RPATH drops it.
          #
          # The nix ld wrapper used to add a real rpath for every library a
          # link uses. The link driver here (wild-cc, or mold through
          # COMPILER_PATH) does not pass through that wrapper, but it does
          # receive NIX_LDFLAGS. The hook therefore names the directory of
          # every library the linker can find (each -L store directory) and
          # of the compiler runtime (libgcc_s) itself, the same rule as ccid's
          # check-shell.
          NIX_NO_SELF_RPATH = true;
          shellHook = ''
            _rpath_flags=""
            for _flag in $NIX_LDFLAGS; do
              case "$_flag" in
                -L${builtins.storeDir}/*)
                  case " $_rpath_flags " in
                    *" -rpath ''${_flag#-L} "*) ;;
                    *) _rpath_flags="$_rpath_flags -rpath ''${_flag#-L}" ;;
                  esac
                  ;;
              esac
            done
            export NIX_LDFLAGS="-rpath ${pkgs.lib.getLib pkgs.stdenv.cc.cc}/lib$_rpath_flags $NIX_LDFLAGS"
            unset _rpath_flags _flag
          '';

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
