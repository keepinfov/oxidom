{
  pkgs,
  lib,
  oxidom-cli,
  oxidom-gui,
}: let
  python = pkgs.python3.withPackages (packages: [packages.pygobject3 packages.xlib]);
in
  pkgs.runCommand "oxidom-gui-smoke" {
    nativeBuildInputs = [python pkgs.glib];
    # GI only drives the private D-Bus connection; GTK runs in the real GUI.
    GI_TYPELIB_PATH = "${pkgs.glib.out}/lib/girepository-1.0";
  } ''
    ${python}/bin/python3 ${../../scripts/gui-smoke.py} \
      --cli ${oxidom-cli}/bin/oxidom \
      --gui ${oxidom-gui}/bin/oxidom-gui \
      --dbus ${pkgs.dbus}/bin/dbus-daemon \
      --xvfb ${pkgs.xvfb}/bin/Xvfb \
      --atspi-registry ${pkgs.at-spi2-core}/libexec/at-spi2-registryd \
      --tool-path ${lib.makeBinPath [pkgs.coreutils pkgs.glib pkgs.dbus]}
    touch $out
  ''
