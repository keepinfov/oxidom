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
        oxidom-cli = pkgs.rustPlatform.buildRustPackage {
          pname = "oxidom";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = ["-p" "oxidom"];
          cargoTestFlags = ["-p" "oxidom" "-p" "oxidom-core"];
          nativeBuildInputs = with pkgs; [pkg-config makeWrapper];
          # Without wrapGAppsHook4 there is no gappsWrapperArgs, so point the daemon at
          # the nix-provided core by hand: an unwrapped binary falls back to `xray` on
          # $PATH and a systemd unit has none.
          postFixup = ''
            wrapProgram $out/bin/oxidom \
              --set-default OXIDOM_XRAY_BIN ${pkgs.xray}/bin/xray \
              --set-default OXIDOM_TUN2SOCKS_BIN ${pkgs.tun2socks}/bin/tun2socks \
              --set-default OXIDOM_NFT_BIN ${pkgs.nftables}/bin/nft
          '';
          postInstall = ''
            install -Dm444 data/dev.keepinfov.oxidom.Daemon.conf \
              $out/share/dbus-1/system.d/dev.keepinfov.oxidom.Daemon.conf
            install -Dm444 data/dev.keepinfov.oxidom.Daemon.service \
              $out/share/dbus-1/system-services/dev.keepinfov.oxidom.Daemon.service
          '';
        };
        oxidom-gui = pkgs.rustPlatform.buildRustPackage {
          pname = "oxidom-gui";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          cargoBuildFlags = ["-p" "oxidom-gui"];
          nativeBuildInputs = with pkgs; [pkg-config wrapGAppsHook4];
          # adwaita-icon-theme is a runtime dependency, not a link-time one:
          # naming it here is what puts it on the wrapper's XDG_DATA_DIRS. Left
          # out, every symbolic icon in the app falls back to a broken square on
          # a target that has no icon theme installed system-wide.
          buildInputs = with pkgs; [gtk4 libadwaita glib adwaita-icon-theme];
          preFixup = ''
            gappsWrapperArgs+=(
              --set-default OXIDOM_XRAY_BIN ${pkgs.xray}/bin/xray
              --set-default OXIDOM_BIN ${oxidom-cli}/bin/oxidom
            )
          '';
          postInstall = ''
            install -Dm444 data/dev.keepinfov.oxidom.svg \
              $out/share/icons/hicolor/scalable/apps/dev.keepinfov.oxidom.svg
            install -Dm444 data/dev.keepinfov.oxidom-symbolic.svg \
              $out/share/icons/hicolor/symbolic/apps/dev.keepinfov.oxidom-symbolic.svg
            install -Dm444 data/icons/oxidom-funnel-symbolic.svg \
              $out/share/icons/hicolor/scalable/actions/oxidom-funnel-symbolic.svg
            install -Dm444 data/dev.keepinfov.oxidom.desktop \
              $out/share/applications/dev.keepinfov.oxidom.desktop
            install -Dm444 data/dev.keepinfov.oxidom.metainfo.xml \
              $out/share/metainfo/dev.keepinfov.oxidom.metainfo.xml
          '';
        };
        oxidom = pkgs.symlinkJoin {
          name = "oxidom";
          paths = [oxidom-cli oxidom-gui];
        };
      in {
        packages.oxidom-cli = oxidom-cli;
        packages.oxidom-gui = oxidom-gui;
        packages.oxidom = oxidom;
        packages.default = oxidom;
        apps.default = {
          type = "app";
          program = "${oxidom-gui}/bin/oxidom-gui";
        };
        checks = {
          cli = oxidom-cli;
          gui = oxidom-gui;
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
            tun2socks
            nftables
          ];
          # In the dev shell, find Xray on PATH from the shell.
          shellHook = ''
            export OXIDOM_XRAY_BIN=${pkgs.xray}/bin/xray
            export OXIDOM_TUN2SOCKS_BIN=${pkgs.tun2socks}/bin/tun2socks
            export OXIDOM_NFT_BIN=${pkgs.nftables}/bin/nft
          '';
        };
        formatter = pkgs.alejandra;
      };
      flake.nixosModules.default = import ./nix/module.nix self;
    };
}
