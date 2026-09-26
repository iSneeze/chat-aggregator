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
