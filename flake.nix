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
            openssl        # likely needed for reqwest/tungstenite (TLS)
            cmake          # some crates need it transitively
          ];

          # helps openssl-sys / other -sys crates find system libs
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.openssl ];

          shellHook = ''
            echo "chat-aggregator dev shell — $(rustc --version)"
          '';
        };
      });
}
