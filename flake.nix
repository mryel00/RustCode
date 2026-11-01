{
  description = "RustCode: a code editor in the spirit of VS Code, written in Rust (Tauri + Monaco)";

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
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # Libraries Tauri needs at build and run time on Linux.
      linuxLibs =
        pkgs: with pkgs; [
          glib-networking
          gtk3
          librsvg
          libsoup_3
          openssl
          webkitgtk_4_1
        ];
    in
    {
      packages = forAllSystems (pkgs: {
        default = pkgs.rustPlatform.buildRustPackage (finalAttrs: {
          pname = "rustcode";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.difference ./. (
              pkgs.lib.fileset.unions (
                [
                  ./flake.nix
                  ./docs
                  ./README.md
                ]
                ++ pkgs.lib.optional (builtins.pathExists ./flake.lock) ./flake.lock
              )
            );
          };

          cargoLock.lockFile = ./Cargo.lock;

          npmDeps = pkgs.fetchNpmDeps {
            name = "${finalAttrs.pname}-${finalAttrs.version}-npm-deps";
            inherit (finalAttrs) src;
            hash = "sha256-JfApeQoB1D3oc+LG4Tnnb6On5sWFp7UL6jEmLp7qVak=";
          };

          nativeBuildInputs =
            with pkgs;
            [
              cargo-tauri.hook
              nodejs
              npmHooks.npmConfigHook
              pkg-config
            ]
            ++ lib.optional stdenv.hostPlatform.isLinux wrapGAppsHook3;

          buildInputs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux (linuxLibs pkgs);

          # `cargo tauri build` runs from the app crate; the workspace root holds Cargo.lock.
          buildAndTestSubdir = "src-tauri";

          # Tests need a display (Tauri) and optionally rust-analyzer; run them in the dev shell.
          doCheck = false;

          meta = {
            description = "Code editor in the spirit of VS Code, with its core written in Rust";
            homepage = "https://github.com/mryel00/RustCode";
            license = pkgs.lib.licenses.mit;
            mainProgram = "rustcode";
            platforms = systems;
          };
        });
      });

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = pkgs.lib.getExe self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          meta.description = "Run RustCode";
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.default ];
          packages = with pkgs; [
            cargo-tauri
            clippy
            rust-analyzer
            rustfmt
          ];
          env = {
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          }
          // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
            # TLS for the webview and a working WebKit renderer on more GPUs.
            GIO_MODULE_DIR = "${pkgs.glib-networking}/lib/gio/modules";
            WEBKIT_DISABLE_DMABUF_RENDERER = "1";
          };
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
