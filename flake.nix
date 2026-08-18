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
        # Cargo.toml is the one place the version is written. Repeating it here
        # meant a release had to remember two files, and a release that forgot
        # produced packages whose name disagreed with the binary inside them.
        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
        oxidom-cli = pkgs.rustPlatform.buildRustPackage {
          pname = "oxidom";
          inherit version;
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
          # Two binaries end up in the joined package, so nothing can infer which
          # one `nix run` and `nix bundle` mean. Naming it is also what lets the
          # AppImage be built straight from this derivation.
          meta.mainProgram = "oxidom";
        };
        oxidom-gui = pkgs.rustPlatform.buildRustPackage {
          pname = "oxidom-gui";
          inherit version;
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
          meta.mainProgram = "oxidom-gui";
        };
        oxidom = pkgs.symlinkJoin {
          name = "oxidom";
          paths = [oxidom-cli oxidom-gui];
          # symlinkJoin does not inherit meta from its inputs, so without this
          # the joined package has no main program and `nix run` and `nix bundle`
          # cannot tell which of the two binaries is meant. The AppImage is built
          # from this attribute precisely because it carries both.
          meta.mainProgram = "oxidom-gui";
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

            # Format on commit instead of in review. Opt out with
            # `git config --unset core.hooksPath`.
            if git rev-parse --git-dir >/dev/null 2>&1 \
              && [ -z "$(git config --get core.hooksPath || true)" ]; then
              git config core.hooksPath .githooks
            fi
          '';
        };
        formatter = pkgs.alejandra;
      };
      flake.nixosModules.default = import ./nix/module.nix self;
    };
}
