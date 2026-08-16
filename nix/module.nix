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

    trayAutostart = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Start `oxidom-gui --background` with the graphical session, so the
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

      systemd.user.services.oxidom-tray = lib.mkIf cfg.trayAutostart {
        description = "oxidom tray and background GUI";
        wantedBy = ["graphical-session.target"];
        partOf = ["graphical-session.target"];
        after = ["graphical-session.target"];
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
