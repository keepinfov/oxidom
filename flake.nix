{
  description = "oxidom - oxided freedom: a GTK4/libadwaita Xray client for Linux";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    crane.url = "github:ipetkov/crane";
  };

  outputs = {
    self,
    flake-parts,
    crane,
    ...
  } @ inputs:
    flake-parts.lib.mkFlake {inherit inputs;} {
      systems = ["x86_64-linux" "aarch64-linux" "aarch64-darwin"];
      perSystem = {
        pkgs,
        lib,
        ...
      }: let
        # Cargo.toml is the one place the version is written. Repeating it here
        # meant a release had to remember two files, and a release that forgot
        # produced packages whose name disagreed with the binary inside them.
        version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

        # Spliced against this flake's nixpkgs, so the builds, the dev shell and
        # CI all compile with the same toolchain rather than a second one pinned
        # by crane's own lock.
        craneLib = crane.mkLib pkgs;

        # `cleanCargoSource` keeps only cargo's inputs, so the data/ assets the
        # packages install (icons, desktop entry, metainfo, D-Bus policy) would
        # be dropped and the postInstall steps below would have nothing to copy.
        # Start from the default source filter — which strips VCS, editor and
        # generated files — then keep cargo sources plus everything under data/.
        src = lib.cleanSourceWith {
          src = ./.;
          filter = path: type:
            lib.cleanSourceFilter path type
            && (craneLib.filterCargoSources path type
              || lib.hasPrefix "${toString ./.}/data" (toString path));
        };

        cliCommon = {
          inherit src version;
          pname = "oxidom";
          nativeBuildInputs = with pkgs; [pkg-config makeWrapper];
        };

        cliDeps = craneLib.buildDepsOnly (cliCommon
          // {
            # Deps for the two crates the CLI builds and tests; oxidom-gui's gtk
            # stack stays out of the headless package.
            cargoExtraArgs = "--locked -p oxidom -p oxidom-core";
          });

        oxidom-cli = craneLib.buildPackage (cliCommon
          // {
            cargoArtifacts = cliDeps;
            cargoBuildExtraArgs = "-p oxidom";
            cargoTestExtraArgs = "-p oxidom -p oxidom-core";
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
          });

        guiCommon = {
          inherit src version;
          pname = "oxidom-gui";
          nativeBuildInputs = with pkgs; [pkg-config wrapGAppsHook4];
          # adwaita-icon-theme is a runtime dependency, not a link-time one:
          # naming it here is what puts it on the wrapper's XDG_DATA_DIRS. Left
          # out, every symbolic icon in the app falls back to a broken square on
          # a target that has no icon theme installed system-wide.
          buildInputs = with pkgs; [gtk4 libadwaita glib adwaita-icon-theme];
        };

        guiDeps = craneLib.buildDepsOnly (guiCommon
          // {
            cargoExtraArgs = "--locked -p oxidom-gui";
          });

        oxidom-gui = craneLib.buildPackage (guiCommon
          // {
            cargoArtifacts = guiDeps;
            cargoBuildExtraArgs = "-p oxidom-gui";
            # Named for the same reason the CLI derivation names its own: without
            # it the build compiles the whole workspace, so oxidom and oxidom-core
            # were compiled and run here as well as in the CLI derivation above.
            # Each derivation now tests what it builds, and `cargo test
            # --workspace` in `test.yml` remains the run that covers everything.
            cargoTestExtraArgs = "-p oxidom-gui";
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
          });

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
