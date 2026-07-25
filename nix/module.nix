self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.oxidom;
  daemonCfg = config.services.oxidom;
  pkg = self.packages.${pkgs.stdenv.hostPlatform.system}.oxidom;
in {
  options.programs.oxidom = {
    enable = lib.mkEnableOption "oxidom, a GTK4 Xray client";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkg;
      description = "The oxidom package to use.";
    };

    trayAutostart = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Start `oxidom gui --background` with the graphical session, so the
        tray icon (and the GNOME system-proxy toggle) are present before the
        window is ever opened.
      '';
    };
  };

  options.services.oxidom = {
    enable = lib.mkEnableOption ''
      the oxidom system daemon: owns the Xray tunnel independently of any
      GUI session, starts at boot, serves D-Bus (dev.keepinfov.oxidom.Daemon)
    '';

    package = lib.mkOption {
      type = lib.types.package;
      default = pkg;
      description = "The oxidom package the daemon runs from.";
    };

    socksPort = lib.mkOption {
      type = lib.types.port;
      default = 10808;
      description = "Local SOCKS5 inbound port (127.0.0.1).";
    };

    httpPort = lib.mkOption {
      type = lib.types.port;
      default = 10809;
      description = "Local HTTP proxy inbound port (127.0.0.1).";
    };

    users = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [];
      example = ["alice"];
      description = ''
        Accounts added to the `oxidom` group, i.e. allowed to drive the daemon
        over the system bus: connect, disconnect, edit subscriptions and change
        the machine's proxy settings.

        Members of `wheel` and root are already allowed by the D-Bus policy —
        they can reach the same capabilities through sudo anyway — so this is
        only needed for accounts that are not administrators.
      '';
    };
  };

  config = lib.mkMerge [
    (lib.mkIf cfg.enable {
      environment.systemPackages = [cfg.package];

      systemd.user.services.oxidom-tray = lib.mkIf cfg.trayAutostart {
        description = "oxidom tray and background GUI";
        wantedBy = ["graphical-session.target"];
        partOf = ["graphical-session.target"];
        after = ["graphical-session.target"];
        serviceConfig = {
          ExecStart = "${cfg.package}/bin/oxidom gui --background";
          Restart = "on-failure";
          RestartSec = 3;
        };
      };
    })

    (lib.mkIf daemonCfg.enable {
      users.users.oxidom = {
        isSystemUser = true;
        group = "oxidom";
        description = "oxidom tunnel daemon";
      };
      users.groups.oxidom.members = daemonCfg.users;

      # Lets the daemon own its system-bus name and users talk to it.
      services.dbus.packages = [daemonCfg.package];

      systemd.services.oxidom = {
        description = "oxidom Xray tunnel daemon";
        wantedBy = ["multi-user.target"];
        wants = ["network-online.target"];
        after = ["network-online.target" "dbus.service"];
        serviceConfig = {
          ExecStart = lib.concatStringsSep " " [
            "${daemonCfg.package}/bin/oxidom"
            "daemon"
            "--system"
            "--socks-port"
            (toString daemonCfg.socksPort)
            "--http-port"
            (toString daemonCfg.httpPort)
          ];
          User = "oxidom";
          Group = "oxidom";
          StateDirectory = "oxidom";
          Restart = "on-failure";
          RestartSec = 2;
          NoNewPrivileges = true;
          ProtectHome = true;
          ProtectSystem = "strict";
          PrivateTmp = true;
        };
      };
    })
  ];
}
