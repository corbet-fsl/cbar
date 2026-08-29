self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.cbar;
  defaultCbarPackage = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
  jsonFormat = pkgs.formats.json {};
  inherit
    (lib)
    types
    mkOption
    mkEnableOption
    mkIf
    getExe
    ;
in {
  options.programs.cbar = {
    enable = mkEnableOption "cbar desktop panel";

    package = mkOption {
      type = types.nullOr types.package;
      default = defaultCbarPackage;
      apply = pkg: if pkg == null then null else pkg.override {features = cfg.features;};
      description = "The package for cbar to use.";
    };

    systemd = mkEnableOption "systemd service for cbar.";

    style = mkOption {
      type = types.either (types.lines) (types.path);
      default = "";
      description = "The stylesheet to apply to cbar.";
    };

    config = mkOption {
      type = jsonFormat.type;
      default = null;
      description = "The config to pass to cbar.";
    };

    features = mkOption {
      type = types.listOf types.nonEmptyStr;
      default = [];
      description = "The features to be used.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.systemd -> cfg.package != null;
        message = "`programs.cbar.systemd` cannot be true when `programs.cbar.package` is null";
      }
    ];

    home.packages = lib.optionals (cfg.package != null) [cfg.package];

    xdg.configFile = {
      "cbar/config.json" = mkIf (cfg.config != null) {
        onChange = lib.mkIf (cfg.package != null) "${getExe cfg.package} reload";
        source = jsonFormat.generate "cbar-config" cfg.config;
      };

      "cbar/style.css" = mkIf (cfg.style != "") (
        if builtins.isPath cfg.style || lib.isStorePath cfg.style
        then {source = cfg.style;}
        else {text = cfg.style;}
      );
    };

    systemd.user.services.cbar = mkIf cfg.systemd {
      Unit = {
        Description = "Cbar desktop panel";
        Documentation = "https://github.com/corbet-labs/cbar";
        PartOf = [
          config.wayland.systemd.target
          "tray.target"
        ];
        After = [config.wayland.systemd.target];
        ConditionEnvironment = "WAYLAND_DISPLAY";
      };

      Service = {
        ExecReload = "${getExe cfg.package} reload";
        ExecStart = "${getExe cfg.package}";
        KillMode = "mixed";
        Restart = "on-failure";
      };

      Install.WantedBy = [
        config.wayland.systemd.target
        "tray.target"
        (mkIf config.wayland.windowManager.hyprland.enable "hyprland-session.target")
        (mkIf config.wayland.windowManager.sway.enable "sway-session.target")
        (mkIf config.wayland.windowManager.river.enable "river-session.target")
      ];
    };
  };
}
