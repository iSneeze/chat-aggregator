{
  description = "chat-aggregator dev environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Native libraries the GPUI desktop app (crates/chat-app) needs on
        # Linux. Some are only loaded at runtime (dlopen: Vulkan, Wayland,
        # xkbcommon, X11), and NixOS has no global /usr/lib to find them in,
        # hence LD_LIBRARY_PATH below. macOS/Windows use system frameworks.
        guiLibs = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux (with pkgs; [
          fontconfig
          freetype
          wayland
          libxkbcommon
          vulkan-loader
          libx11
          libxcb
          libxcursor
          libxrandr
          libxi
        ]);

        cargoToml = pkgs.lib.importTOML ./Cargo.toml;

        # The desktop app as a package: `nix run github:iSneeze/chat-aggregator`
        # starts it, `nix build` leaves it in ./result. Built with nixpkgs'
        # stable Rust (the code needs no nightly features), from Cargo.lock:
        # Nix downloads every crate up front and builds offline.
        chatAggregator = pkgs.rustPlatform.buildRustPackage {
          pname = "chat-aggregator";
          version = cargoToml.workspace.package.version;

          # Only what the build reads: editing the docs doesn't rebuild.
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./.cargo
              ./crates
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;

          # Only the app (and what it depends on), not the examples.
          cargoBuildFlags = [ "-p" "chat-app" ];
          # The tests run in the dev shell and before every commit; here
          # they'd only make each package build slower.
          doCheck = false;

          nativeBuildInputs = with pkgs; [
            pkg-config
            protobuf      # chat-youtube compiles its gRPC schema at build time
            cmake         # aws-lc-sys (TLS crypto) may build with it
            makeWrapper
          ];
          buildInputs = guiLibs;

          # The libraries GPUI only loads while running (Vulkan, Wayland,
          # xkbcommon, X11) aren't linked, so nothing records where they
          # are; on NixOS there's no /usr/lib to fall back on. The wrapper
          # tells the program, like LD_LIBRARY_PATH in the dev shell below.
          postFixup = ''
            wrapProgram $out/bin/chat-aggregator \
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath guiLibs}
          '';

          meta = {
            description = "Twitch, YouTube and Picarto live chat in one overlay for your stream";
            homepage = "https://isneeze.github.io/chat-aggregator/";
            license = pkgs.lib.licenses.mit;
            mainProgram = "chat-aggregator";
            # Windows and macOS builds come from GitHub Actions.
            platforms = pkgs.lib.platforms.linux;
          };
        };

        rustToolchain = pkgs.rust-bin.nightly.latest.default.override {
          extensions = [
            "rust-src"
            "rust-analyzer"
            "miri"
            "clippy"
            "rustfmt"
          ];
        };
      in
      {
        packages.default = chatAggregator;

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustToolchain
            pkg-config
            openssl
            mold
            bacon
            cargo-flamegraph
            cargo-expand
            cargo-seek
            cargo-generate
            cargo-outdated
            cargo-audit
            cargo-about    # third-party license notices (about.toml)
            cargo-watch
            cargo-nextest
            rustlings
            taplo
            protobuf       # required by youtube gRPC connection
            websocat       # manual testing of the JSON WebSocket API
            cmake          # some crates need it transitively
          ] ++ guiLibs;

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath guiLibs;

          shellHook = ''
            echo "chat-aggregator dev shell — $(rustc --version)"
          '';
        };
      });
}
