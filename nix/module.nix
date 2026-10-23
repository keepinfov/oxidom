self: {
  config,
  lib,
  pkgs,
  ...
}: let
  cfg = config.programs.oxidom;
  daemonCfg = config.services.oxidom;
  guiPkg = self.packages.${pkgs.stdenv.hostPlatform.system}.oxidom-gui;
  cliPkg = self.packages.${pkgs.stdenv.hostPlatform.system}.oxidom-cli;
in {
  options.programs.oxidom = {
    enable = lib.mkEnableOption "oxidom, a GTK4 Xray client";

    package = lib.mkOption {
      type = lib.types.package;
      default = guiPkg;
      description = "The oxidom package to use.";
    };

    sessionDaemon = {
      enable = lib.mkEnableOption ''
        a separately supervised, unprivileged oxidom session daemon, independent
        of the GUI and tray. It uses the user's normal XDG configuration and data
        roots and restarts on failure, with rate limiting. Only eligible pool
        sessions can be adopted during recovery; not every tunnel survives a
        crash. It runs with the user manager, without enabling lingering.
        By default clients start a standalone daemon without crash restart
      '';

      package = lib.mkOption {
        type = lib.types.package;
        default = cliPkg;
        description = "The oxidom package the unprivileged session daemon runs from.";
      };
    };

    trayAutostart = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Start `oxidom-gui --background` with the graphical session, so the
        tray icon (and the GNOME system-proxy toggle) are present before the
        window is ever opened. This does not enable the managed session daemon:
        by default its standalone daemon has no crash restart. Enable
        `programs.oxidom.sessionDaemon.enable` for supervision independent of
        the GUI and tray.
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
      default = cliPkg;
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

    tun.enable = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Allow profile TUN interfaces. This grants only the oxidom system daemon
        CAP_NET_ADMIN and keeps NetworkManager away from oxi-* devices.
      '';
    };
  };

  config = lib.mkMerge [
    (lib.mkIf cfg.enable {
      environment.systemPackages = [cfg.package cliPkg];

      environment.sessionVariables = lib.mkIf cfg.sessionDaemon.enable {
        OXIDOM_SESSION_DAEMON = "external";
      };

      # Share the packaged unit; only its binary path and declarative enablement
      # differ on NixOS. This is a user-manager service, not a GUI dependency.
      systemd.user.units."oxidom-session.service" = lib.mkIf cfg.sessionDaemon.enable {
        wantedBy = ["default.target"];
        text =
          lib.replaceStrings ["/usr/bin/oxidom"] ["${cfg.sessionDaemon.package}/bin/oxidom"]
          (builtins.readFile ../packaging/systemd/oxidom-session.service);
      };

      systemd.user.services.oxidom-tray = lib.mkIf cfg.trayAutostart {
        description = "oxidom tray and background GUI";
        wantedBy = ["graphical-session.target"];
        partOf = ["graphical-session.target"];
        after = ["graphical-session.target"] ++ lib.optional cfg.sessionDaemon.enable "oxidom-session.service";
        wants = lib.optional cfg.sessionDaemon.enable "oxidom-session.service";
        environment = lib.mkIf cfg.sessionDaemon.enable {
          OXIDOM_SESSION_DAEMON = "external";
        };
        serviceConfig = {
          ExecStart = "${cfg.package}/bin/oxidom-gui --background";
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

      # Lets the daemon own its system-bus name, users talk to it, and a client
      # that asks for the name while the unit is still starting *wait* for it
      # (share/dbus-1/system-services) instead of racing it.
      services.dbus.packages = [daemonCfg.package];
      networking.networkmanager.unmanaged = lib.mkIf daemonCfg.tun.enable [
        "interface-name:oxi-*"
      ];

      systemd.services.oxidom = {
        description = "oxidom Xray tunnel daemon";
        wantedBy = ["multi-user.target"];
        wants = ["network-online.target"];
        after = ["network-online.target" "dbus.service"];
        path = [pkgs.iputils];
        serviceConfig =
          {
            # D-Bus activation: the unit counts as started once it owns the name,
            # so a client's first call blocks until the daemon can answer it
            # rather than falling through to a session daemon of its own.
            Type = "dbus";
            BusName = "dev.keepinfov.oxidom.Daemon";
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
            # The Xray cores carry the traffic; the daemon only supervises them.
            # Under the default control-group killing, a daemon crash takes every
            # tunnel down with it and the restarted daemon has nothing left to
            # adopt — including the `default` session that redsocks points at.
            # Only the main process is signalled, so a crash costs a few seconds
            # of supervision rather than the connection. A clean stop still tears
            # the cores down, because the daemon's own SIGTERM handler does it;
            # anything that does leak is reaped by `recover()` on the next start.
            KillMode = "process";
            NoNewPrivileges = true;
            ProtectHome = true;
            ProtectSystem = "strict";
            PrivateTmp = true;
          }
          // lib.optionalAttrs daemonCfg.tun.enable {
            AmbientCapabilities = ["CAP_NET_ADMIN"];
            CapabilityBoundingSet = ["CAP_NET_ADMIN"];
            RestrictAddressFamilies = ["AF_UNIX" "AF_INET" "AF_INET6" "AF_NETLINK"];
          };
      };

      # Template unit: instantiated as `oxidom@<profile>`. It deliberately has
      # no `wantedBy` — NixOS would link the template itself into
      # multi-user.target.wants, and systemd cannot start a template with no
      # instance. Enable the instances you want instead, e.g.
      # `systemd.services."oxidom@work".wantedBy = ["multi-user.target"];`
      systemd.services."oxidom@" = {
        description = "oxidom profile %i";
        requires = ["oxidom.service"];
        after = ["oxidom.service"];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${daemonCfg.package}/bin/oxidom up %i";
          ExecStop = "${daemonCfg.package}/bin/oxidom down %i";
        };
      };
    })
  ];
}
