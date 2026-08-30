{
  gtk4,
  gdk-pixbuf,
  librsvg,
  webp-pixbuf-loader,
  gobject-introspection,
  glib-networking,
  glib,
  shared-mime-info,
  gsettings-desktop-schemas,
  wrapGAppsHook4,
  gtk4-layer-shell,
  gnome,
  libxkbcommon,
  libpulseaudio,
  libinput,
  libevdev,
  luajit,
  luajitPackages,
  pkg-config,
  installShellFiles,
  adwaita-icon-theme,
  hicolor-icon-theme,
  lib,
  version ? "git",
  features ? [],
  craneLib,
  dbus,
}: let
  hasFeature = f: features == [] || builtins.elem f features;
  flags = let
    noDefault =
      if features == []
      then ""
      else "--no-default-features";

    featuresStr =
      if features == []
      then ""
      else ''-F "${builtins.concatStringsSep "," features}"'';
  in [
    noDefault
    featuresStr
  ];
  lgi = luajitPackages.lgi;
  gappsWrapperArgs =
    ''
      # Thumbnailers
          --prefix XDG_DATA_DIRS : "${gdk-pixbuf}/share"
          --prefix XDG_DATA_DIRS : "${librsvg}/share"
          --prefix XDG_DATA_DIRS : "${webp-pixbuf-loader}/share"
          --prefix XDG_DATA_DIRS : "${shared-mime-info}/share"

          # gtk-launch
          --suffix PATH : "${lib.makeBinPath [gtk4]}"
    ''
    + lib.optionalString (hasFeature "cairo") ''
      --prefix LUA_PATH : "./?.lua;${lgi}/share/lua/5.1/?.lua;${lgi}/share/lua/5.1/?/init.lua;${luajit}/share/lua/5.1/\?.lua;${luajit}/share/lua/5.1/?/init.lua"
      --prefix LUA_CPATH : "./?.so;${lgi}/lib/lua/5.1/?.so;${luajit}/lib/lua/5.1/?.so;${luajit}/lib/lua/5.1/loadall.so"
    '';
in
  craneLib.buildPackage {
    inherit version;

    pname = "cbar";

    # CI already handles checks
    doCheck = false;

    src = let
      fs = lib.fileset;
      root = ../.;

      # Keep this list explicit: the package source is a cache boundary, not a
      # repository archive. Crane keeps every workspace manifest and the lock
      # file, while the remaining entries are exactly the Rust sources and
      # non-Rust inputs consumed by cargo, rustc, or postInstall.
      cargoSources = fs.unions [
        (craneLib.fileset.cargoTomlAndLock root)
        (lib.path.append root ".cargo/config.toml")
        (lib.path.append root "build.rs")
        (lib.path.append root "src")
        (lib.path.append root "launch-service/src")
        (lib.path.append root "launcher-core/src")
        (lib.path.append root "launcher-gtk/src")
      ];

      # These files are embedded with include_str!. Changes to them therefore
      # change the application and must invalidate the final package.
      embeddedInputs = fs.unions [
        (lib.path.append root "README.md")
        (lib.path.append root "docs/Dynamic values.md")
        (lib.path.append root "docs/Ironvars.md")
        (lib.path.append root "lua/init.lua")
        (lib.path.append root "lua/draw.lua")
        (lib.path.append root "examples/minimal/config.corn")
        (lib.path.append root "examples/minimal/config.json")
        (lib.path.append root "examples/minimal/config.toml")
        (lib.path.append root "examples/minimal/config.yaml")
        (lib.path.append root "examples/minimal/style.css")
        (lib.path.append root "examples/desktop/config.corn")
        (lib.path.append root "examples/desktop/config.json")
        (lib.path.append root "examples/desktop/config.toml")
        (lib.path.append root "examples/desktop/config.yaml")
        (lib.path.append root "examples/desktop/style.css")
      ];

      # These are copied verbatim into the package output below. Test fixtures
      # are deliberately absent: their include_str! calls are cfg(test), and
      # this package derivation intentionally has doCheck = false.
      installedLegalInputs = fs.unions [
        (lib.path.append root "LICENSE")
        (lib.path.append root "LICENSES/IRONBAR-MIT.txt")
        (lib.path.append root "launcher-core/LICENSE")
        (lib.path.append root "NOTICE")
      ];
    in
      fs.toSource {
        inherit root;
        fileset = fs.unions [
          cargoSources
          embeddedInputs
          installedLegalInputs
        ];
      };

    nativeBuildInputs = [
      pkg-config
      wrapGAppsHook4
      gobject-introspection
      installShellFiles
    ];

    buildInputs =
      [
        gtk4
        gdk-pixbuf
        glib
        gtk4-layer-shell
        glib-networking
        shared-mime-info
        adwaita-icon-theme
        hicolor-icon-theme
        gsettings-desktop-schemas
        libxkbcommon
        dbus
      ]
      ++ lib.optionals (hasFeature "volume") [libpulseaudio]
      ++ lib.optionals (hasFeature "cairo") [luajit]
      ++ lib.optionals (hasFeature "keyboard") [
        libinput
        libevdev
      ];

    propagatedBuildInputs = [gtk4];

    cargoExtraArgs = builtins.concatStringsSep " " (builtins.filter (s: s != "") flags);

    preFixup = ''
      gappsWrapperArgs+=(
        ${gappsWrapperArgs}
      )
    '';

    postInstall = ''
      mkdir -p target/completions
      target/release/cbar --print-completions bash > target/completions/cbar.bash
      target/release/cbar --print-completions zsh > target/completions/_cbar
      target/release/cbar --print-completions fish > target/completions/cbar.fish

      installShellCompletion --cmd cbar \
        --bash target/completions/cbar.bash \
        --fish target/completions/cbar.fish \
        --zsh target/completions/_cbar

      install -Dm644 LICENSE "$out/share/licenses/cbar/LICENSE"
      install -Dm644 LICENSES/IRONBAR-MIT.txt \
        "$out/share/licenses/cbar/LICENSE.ironbar"
      install -Dm644 launcher-core/LICENSE \
        "$out/share/licenses/cbar/LICENSE.nixlaunch"
      install -Dm644 NOTICE "$out/share/doc/cbar/NOTICE"
    '';

    meta = {
      homepage = "https://github.com/corbet-labs/cbar";
      description = "Opinionated GTK4 desktop panel with an integrated launcher.";
      # FSL-1.1-ALv2 has no identifier in nixpkgs' license set. It is
      # source-available and converts to Apache-2.0 two years per release.
      license = {
        fullName = "Functional Source License, Version 1.1, ALv2 Future License";
        url = "https://fsl.software/FSL-1.1-ALv2.template.md";
        free = false;
        redistributable = true;
      };
      platforms = lib.platforms.linux;
      mainProgram = "cbar";
    };
  }
