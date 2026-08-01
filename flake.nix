{
  description = "Agent Manager — multi-instance cursor-agent GUI (vidya + egui_term)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Fetched for pure `nix build` / `nix profile add` (git+file cannot see
    # siblings). Local cargo apps still use Cargo.toml path = "../vidya".
    # Override while hacking: --override-input vidya path:../vidya
    vidya = {
      url = "git+https://tangled.org/nandi.uk/vidya";
      flake = false;
    };
  };

  outputs =
    { self, nixpkgs, vidya }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      eguiLibs =
        pkgs:
        with pkgs;
        [
          libxkbcommon
          libGL
          vulkan-loader
        ]
        ++ lib.optionals stdenv.hostPlatform.isLinux [
          wayland
          libx11
          libxcursor
          libxi
          libxrandr
        ];

      # Shared by cargo apps: enter checkout + set link path.
      cargoPreamble = libPath: ''
        set -euo pipefail
        export LD_LIBRARY_PATH="${libPath}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

        if [ ! -f Cargo.toml ] && [ -f "''${FLAKE_ROOT:-}/Cargo.toml" ]; then
          cd "$FLAKE_ROOT"
        fi
        if [ ! -f Cargo.toml ]; then
          echo "manager: no Cargo.toml here (cwd=$PWD)" >&2
          echo "  cd into the manager checkout, then: nix run .#manager" >&2
          exit 1
        fi
        if [ ! -d ../vidya ]; then
          echo "manager: expected sibling ../vidya (Cargo path dep)" >&2
          echo "  clone vidya next to manager, or: nix build .#manager" >&2
          exit 1
        fi
        if [ ! -f assets/manager.svg ] || [ ! -f manager.desktop ]; then
          echo "manager: missing assets/manager.svg or manager.desktop" >&2
          exit 1
        fi
      '';

      # Install FreeDesktop entry + hicolor icons under a share root (usually
      # $XDG_DATA_HOME). Wayland compositors (GNOME) look up icons by app_id via
      # the *session* data dirs — a temporary XDG_DATA_DIRS on the child alone
      # is not enough. Absolute Icon= path is the reliable dock/overview path.
      # Shell fragment: $1=shareDir $2=execPath (absolute).
      installDesktopShareSh = ''
        _share="$1"
        _exec="$2"
        _icon="$_share/icons/hicolor/256x256/apps/manager.png"
        mkdir -p "$_share/applications"
        mkdir -p "$_share/icons/hicolor/scalable/apps"
        cp assets/manager.svg "$_share/icons/hicolor/scalable/apps/manager.svg"
        for sz in 16 24 32 48 64 128 256; do
          mkdir -p "$_share/icons/hicolor/''${sz}x''${sz}/apps"
          rsvg-convert -w "$sz" -h "$sz" assets/manager.svg \
            -o "$_share/icons/hicolor/''${sz}x''${sz}/apps/manager.png"
        done
        sed -e "s|@EXEC@|''${_exec}|g" -e "s|@ICON@|''${_icon}|g" manager.desktop \
          > "$_share/applications/manager.desktop"
        # Help icon themes / menus pick up the new files this session.
        if command -v gtk-update-icon-cache >/dev/null 2>&1; then
          gtk-update-icon-cache -f "$_share/icons/hicolor" 2>/dev/null || true
        fi
        if command -v update-desktop-database >/dev/null 2>&1; then
          update-desktop-database "$_share/applications" 2>/dev/null || true
        fi
      '';
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          inherit (pkgs) lib;
          libs = eguiLibs pkgs;

          # Packaged binary for `nix build` / install — pure, remote-builder capable.
          srcTree = pkgs.runCommand "manager-src" { } ''
            mkdir -p $out/manager $out/vidya
            cp -a ${lib.cleanSource ./.}/. $out/manager/
            cp -a ${vidya}/. $out/vidya/
            chmod -R u+w $out
            rm -rf $out/manager/{target,result,result-*,.git,.jj,.desktop-share} 2>/dev/null || true
            rm -rf $out/vidya/{target,android-demo,host,examples,docs,.git,.jj} 2>/dev/null || true
          '';

          manager = pkgs.rustPlatform.buildRustPackage {
            pname = "manager";
            version = "0.1.0";
            src = srcTree;
            sourceRoot = "manager-src/manager";
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = [
              pkgs.makeWrapper
              pkgs.librsvg
            ];
            buildInputs = libs;

            postInstall = ''
              wrapProgram $out/bin/manager \
                --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath libs}

              install -Dm644 ${./assets/manager.svg} \
                $out/share/icons/hicolor/scalable/apps/manager.svg
              for sz in 16 24 32 48 64 128 256; do
                mkdir -p $out/share/icons/hicolor/''${sz}x''${sz}/apps
                rsvg-convert -w "$sz" -h "$sz" ${./assets/manager.svg} \
                  -o $out/share/icons/hicolor/''${sz}x''${sz}/apps/manager.png
              done

              install -d $out/share/applications
              substitute ${./manager.desktop} $out/share/applications/manager.desktop \
                --replace-fail '@EXEC@' "$out/bin/manager" \
                --replace-fail '@ICON@' "$out/share/icons/hicolor/256x256/apps/manager.png"
            '';

            meta = {
              description = "Multi-instance cursor-agent manager (vidya + egui_term)";
              homepage = "https://tangled.org/nandi.uk/manager";
              license = lib.licenses.mit;
              mainProgram = "manager";
              platforms = lib.platforms.linux;
            };
          };

          # Install store .desktop+icons into the user data home, then gtk-launch.
          # (GNOME Shell only resolves icons from session-known data dirs.)
          desktop = pkgs.writeShellApplication {
            name = "manager-desktop";
            runtimeInputs = [
              pkgs.gtk3
              pkgs.desktop-file-utils
            ];
            text = ''
              set -euo pipefail
              DATA_HOME="''${XDG_DATA_HOME:-$HOME/.local/share}"
              mkdir -p "$DATA_HOME/applications" "$DATA_HOME/icons/hicolor"
              cp -a ${manager}/share/icons/hicolor/. "$DATA_HOME/icons/hicolor/"
              ICON="$DATA_HOME/icons/hicolor/256x256/apps/manager.png"
              sed -e "s|^Exec=.*|Exec=${manager}/bin/manager|" \
                  -e "s|^Icon=.*|Icon=$ICON|" \
                ${manager}/share/applications/manager.desktop \
                > "$DATA_HOME/applications/manager.desktop"
              gtk-update-icon-cache -f "$DATA_HOME/icons/hicolor" 2>/dev/null || true
              update-desktop-database "$DATA_HOME/applications" 2>/dev/null || true
              export XDG_DATA_DIRS="$DATA_HOME''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
              if command -v gtk-launch >/dev/null 2>&1; then
                exec gtk-launch manager "$@"
              fi
              exec ${manager}/bin/manager "$@"
            '';
          };
        in
        {
          default = manager;
          manager = manager;
          desktop = desktop;
        }
      );

      apps = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          inherit (pkgs) lib;
          libs = eguiLibs pkgs;
          libPath = lib.makeLibraryPath libs;
          cargoTools = with pkgs; [
            rustc
            cargo
            pkg-config
            librsvg
            gtk3
            desktop-file-utils # update-desktop-database
          ];

          # Local cargo build (devshell tools; no pure sandbox / remote upload lock).
          build = pkgs.writeShellApplication {
            name = "manager-build";
            runtimeInputs = cargoTools;
            text = ''
              ${cargoPreamble libPath}
              echo "→ cargo build --release $*"
              cargo build --release "$@"
              echo "✓ target/release/manager"
            '';
          };

          # Build (if needed) and run via the same toolchain as `nix develop`.
          managerApp = pkgs.writeShellApplication {
            name = "manager";
            runtimeInputs = cargoTools;
            text = ''
              ${cargoPreamble libPath}
              echo "→ cargo build --release"
              cargo build --release
              echo "→ target/release/manager $*"
              exec ./target/release/manager "$@"
            '';
          };

          # Cargo build, install .desktop+icons into XDG_DATA_HOME (so GNOME/Wayland
          # can resolve Icon= by app_id), then gtk-launch.
          desktopApp = pkgs.writeShellApplication {
            name = "manager-desktop";
            runtimeInputs = cargoTools;
            text = ''
              ${cargoPreamble libPath}
              echo "→ cargo build --release"
              cargo build --release

              EXEC="$PWD/target/release/manager"
              # Session-visible FreeDesktop tree (not a private .desktop-share):
              # Wayland shells ignore child-only XDG_DATA_DIRS for dock icons.
              DATA_HOME="''${XDG_DATA_HOME:-$HOME/.local/share}"
              echo "→ install launcher+icon → $DATA_HOME ({applications,icons}/…)"
              (
                set -- "$DATA_HOME" "$EXEC"
                ${installDesktopShareSh}
              )
              # Mirror under .desktop-share for inspection / CI.
              SHARE="''${MANAGER_DESKTOP_SHARE:-$PWD/.desktop-share}"
              rm -rf "$SHARE"
              mkdir -p "$SHARE"
              (
                set -- "$SHARE" "$EXEC"
                ${installDesktopShareSh}
              )

              export XDG_DATA_DIRS="$DATA_HOME''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
              ICON="$DATA_HOME/icons/hicolor/256x256/apps/manager.png"
              echo "→ gtk-launch manager  (Icon=$ICON, Exec=$EXEC, app_id=manager)"
              if command -v gtk-launch >/dev/null 2>&1; then
                exec gtk-launch manager "$@"
              fi
              exec "$EXEC" "$@"
            '';
          };
        in
        {
          # Default: cargo build + gtk-launch .desktop (icon + FreeDesktop metadata).
          default = {
            type = "app";
            program = "${desktopApp}/bin/manager-desktop";
          };
          desktop = {
            type = "app";
            program = "${desktopApp}/bin/manager-desktop";
          };
          # Binary only (no .desktop / icon path).
          manager = {
            type = "app";
            program = "${managerApp}/bin/manager";
          };
          build = {
            type = "app";
            program = "${build}/bin/manager-build";
          };
          # Pure store package launched via its .desktop (needs nix build).
          package-desktop = {
            type = "app";
            program = "${self.packages.${system}.desktop}/bin/manager-desktop";
          };
          package = {
            type = "app";
            program = "${self.packages.${system}.manager}/bin/manager";
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          libs = eguiLibs pkgs;
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              rustc
              cargo
              rustfmt
              clippy
              pkg-config
              librsvg
            ];
            buildInputs = libs;
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath libs;
            shellHook = ''
              echo "manager — nix run (.desktop+icon) | nix run .#manager | nix run .#build | nix develop"
            '';
          };
        }
      );
    };
}
