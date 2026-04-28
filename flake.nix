{
  description = "Engram — pinned dev toolchain (Rust + just + jq + sqlx-cli + cargo-watch).";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, flake-utils, rust-overlay, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        # Reads rust-toolchain.toml so the flake stays in lockstep with
        # the file rustup picks up. Bumping Rust = edit rust-toolchain.toml,
        # the flake follows.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      in {
        devShells.default = pkgs.mkShell {
          # Per-project tooling. System services (Postgres, Docker daemon)
          # and privileged binaries (Firecracker) live outside the shell —
          # see README "Validating the production path".
          packages = with pkgs; [
            rustToolchain
            just
            jq
            postgresql              # psql + pg_dump; not the server
            sqlx-cli
            cargo-watch
            pkg-config
            openssl
            protobuf                # protoc, for tonic-build when grpc lands
          ] ++ lib.optionals stdenv.isDarwin [
            libiconv                # required by some macOS-aarch64 crates
          ];

          # OPENSSL_DIR / PKG_CONFIG_PATH so `cargo build` finds the
          # Nix-provided openssl instead of looking system-wide.
          env = {
            OPENSSL_DIR = "${pkgs.openssl.dev}";
            OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
            PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
          };

          shellHook = ''
            echo "engram dev shell — rustc $(rustc --version | awk '{print $2}'), just $(just --version | awk '{print $2}'), fc=system"
          '';
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}
