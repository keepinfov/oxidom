{
  description = "oxidom - oxided freedom: a GTK4/libadwaita Xray client for Linux";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
  };

  outputs = {
    self,
    flake-parts,
    ...
  } @ inputs:
    flake-parts.lib.mkFlake {inherit inputs;} {
      systems = ["x86_64-linux" "aarch64-linux" "aarch64-darwin"];
      perSystem = {pkgs, ...}: let
        oxidom = pkgs.rustPlatform.buildRustPackage {
          pname = "oxidom";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = with pkgs; [pkg-config wrapGAppsHook4];
          buildInputs = with pkgs; [gtk4 libadwaita glib];
          # Point the binary at the nix-provided Xray core at build time.
          preFixup = ''
            gappsWrapperArgs+=(--set-default OXIDOM_XRAY_BIN ${pkgs.xray}/bin/xray)
          '';
          postInstall = ''
            install -Dm444 data/dev.keepinfov.oxidom.svg \
              $out/share/icons/hicolor/scalable/apps/dev.keepinfov.oxidom.svg
            install -Dm444 data/dev.keepinfov.oxidom-symbolic.svg \
              $out/share/icons/hicolor/symbolic/apps/dev.keepinfov.oxidom-symbolic.svg
            install -Dm444 data/dev.keepinfov.oxidom.desktop \
              $out/share/applications/dev.keepinfov.oxidom.desktop
            install -Dm444 data/dev.keepinfov.oxidom.metainfo.xml \
              $out/share/metainfo/dev.keepinfov.oxidom.metainfo.xml
            install -Dm444 data/dev.keepinfov.oxidom.Daemon.conf \
              $out/share/dbus-1/system.d/dev.keepinfov.oxidom.Daemon.conf
          '';
        };
      in {
        packages.default = oxidom;
        packages.oxidom = oxidom;
        apps.default = {
          type = "app";
          program = "${oxidom}/bin/oxidom";
        };
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [pkg-config wrapGAppsHook4];
          buildInputs = with pkgs; [
            gtk4
            libadwaita
            glib
            cargo
            rustc
            rust-analyzer
            clippy
            rustfmt
            xray
          ];
          # In the dev shell, find Xray on PATH from the shell.
          shellHook = ''
            export OXIDOM_XRAY_BIN=${pkgs.xray}/bin/xray
          '';
        };
        formatter = pkgs.alejandra;
      };
      flake.nixosModules.default = import ./nix/module.nix self;
    };
}
