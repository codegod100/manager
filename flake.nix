{
  description = "Agent Manager — multi-instance cursor-agent GUI (vidya + egui_term)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: nixpkgs.legacyPackages.${system};

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
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          inherit (pkgs) lib;
          libs = eguiLibs pkgs;
          libPath = lib.makeLibraryPath libs;

          # `cargo run` launcher: rustup's cargo + nix-provided GL/Wayland libs.
          # Prefers a live checkout (cwd); falls back to the flake source in the store.
          # Path-dep `../vidya` requires the manager tree to sit next to a vidya checkout
          # (cwd case). Store fallback only works if that sibling layout is preserved.
          managerRunner = pkgs.writeShellApplication {
            name = "manager";
            text = ''
              export LD_LIBRARY_PATH="${libPath}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
              export PATH="''${HOME}/.cargo/bin:$PATH"

              if ! command -v cargo >/dev/null 2>&1; then
                echo "manager: cargo not found — install rustup or put cargo on PATH" >&2
                exit 127
              fi

              run_manager() {
                local root=$1
                shift
                if [ ! -d "$root/../vidya" ]; then
                  echo "manager: expected sibling checkout at $root/../vidya" >&2
                  echo "  keep manager next to vidya (path dep in Cargo.toml)" >&2
                  exit 1
                fi
                exec cargo run --manifest-path "$root/Cargo.toml" -- "$@"
              }

              # nix run keeps the caller's cwd → live edit/rebuild against the tree
              if [ -f Cargo.toml ] && [ -d src ]; then
                run_manager "$PWD" "$@"
              fi

              # App launcher / nix run from another directory → flake source (read-only)
              flake_src=${lib.escapeShellArg (toString self)}
              if [ -f "$flake_src/Cargo.toml" ]; then
                export CARGO_TARGET_DIR="''${CARGO_TARGET_DIR:-''${XDG_CACHE_HOME:-$HOME/.cache}/manager/cargo-target}"
                run_manager "$flake_src" "$@"
              fi

              echo "manager: Cargo.toml not found" >&2
              echo "  run from the manager checkout (next to ../vidya), or: nix run path:." >&2
              exit 1
            '';
          };
        in
        rec {
          manager = managerRunner;
          default = manager;
        }
      );

      apps = forAllSystems (
        system:
        let
          manager = self.packages.${system}.manager;
        in
        {
          manager = {
            type = "app";
            program = "${manager}/bin/manager";
          };
          default = self.apps.${system}.manager;
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          libs = eguiLibs pkgs;
        in
        {
          default = pkgs.mkShell {
            buildInputs = libs;
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath libs;
            shellHook = ''
              export PATH="$HOME/.cargo/bin:$PATH"
              echo "manager — nix run | cargo run"
            '';
          };
        }
      );
    };
}
