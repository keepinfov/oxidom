{
  self,
  nixpkgs,
  pkgs,
  lib,
}: let
  evaluate = programs:
    (import (nixpkgs + "/nixos/lib/eval-config.nix") {
      system = pkgs.stdenv.hostPlatform.system;
      modules = [
        self.nixosModules.default
        {
          system.stateVersion = "26.05";
          programs.oxidom = programs;
        }
      ];
    }).config;
  defaults = evaluate {enable = true;};
  trayOnly = evaluate {
    enable = true;
    trayAutostart = true;
  };
  managed = evaluate {
    enable = true;
    sessionDaemon.enable = true;
  };
  managedTray = evaluate {
    enable = true;
    sessionDaemon.enable = true;
    trayAutostart = true;
  };
  disabled = evaluate {sessionDaemon.enable = true;};
  noSession = cfg:
    !(cfg.systemd.user.units ? "oxidom-session.service")
    && !(cfg.environment.sessionVariables ? OXIDOM_SESSION_DAEMON);
  tray = managedTray.systemd.user.services.oxidom-tray;
  unit = managed.systemd.user.units."oxidom-session.service";
  lines = lib.splitString "\n" unit.text;
  cliPkg = self.packages.${pkgs.stdenv.hostPlatform.system}.oxidom-cli;
in
  assert lib.assertMsg (lib.all noSession [defaults trayOnly disabled])
  "Standalone clients, including tray autostart, must not opt into managed mode";
  assert lib.assertMsg (
    !(trayOnly.systemd.user.services.oxidom-tray.environment ? OXIDOM_SESSION_DAEMON)
    && !(lib.elem "oxidom-session.service" trayOnly.systemd.user.services.oxidom-tray.after)
    && trayOnly.systemd.user.services.oxidom-tray.wants == []
  ) "Standalone tray autostart must not depend on a managed session daemon";
  assert lib.assertMsg (lib.all (cfg:
    !cfg.services.oxidom.enable
    && !(cfg.systemd.services ? oxidom)
    && !(cfg.users.users ? oxidom)
    && !(lib.elem (toString cliPkg) (map toString cfg.services.dbus.packages)))
  [defaults managed managedTray])
  "Session mode must not enable a system daemon, account, or extra D-Bus policy";
  assert lib.assertMsg (
    unit.wantedBy
    == ["default.target"]
    && !(managed.systemd.user.services ? oxidom-tray)
    && managed.programs.oxidom.sessionDaemon.package == cliPkg
    && managedTray.systemd.user.units."oxidom-session.service".text == unit.text
  ) "The session daemon must use the CLI package and remain independent of the tray";
  assert lib.assertMsg (lib.all (line: lib.elem line lines) [
    "[Unit]"
    "StartLimitIntervalSec=60"
    "StartLimitBurst=3"
    "[Service]"
    "Type=dbus"
    "BusName=dev.keepinfov.oxidom.Daemon"
    "ExecStart=${cliPkg}/bin/oxidom daemon"
    "TimeoutStartSec=180"
    "Restart=on-failure"
    "RestartSec=2"
    "KillMode=process"
    "NoNewPrivileges=true"
    "CapabilityBoundingSet="
    "AmbientCapabilities="
    "WantedBy=default.target"
  ]) "The shared session unit must keep its bus readiness, restart limits, and privilege restrictions";
  assert lib.assertMsg (lib.all (text: !(lib.hasInfix text unit.text)) [
    "--system"
    "StateDirectory="
    "Environment="
    "EnvironmentFile="
    "User="
    "Group="
    "PartOf="
    "Requires="
    "graphical-session.target"
    "CAP_"
  ]) "The session unit must retain user XDG roots without privileges or GUI lifecycle coupling";
  assert lib.assertMsg (
    managed.environment.sessionVariables.OXIDOM_SESSION_DAEMON
    == "external"
    && managedTray.environment.sessionVariables.OXIDOM_SESSION_DAEMON == "external"
    && tray.environment.OXIDOM_SESSION_DAEMON == "external"
    && lib.elem "oxidom-session.service" tray.after
    && tray.wants == ["oxidom-session.service"]
    && tray.requires == []
    && tray.partOf == ["graphical-session.target"]
  ) "GUI, CLI, and tray must use external mode with only weak tray-to-daemon ordering";
    pkgs.runCommand "oxidom-session-daemon-check" {} ''
      touch $out
    ''
