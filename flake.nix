{
  description = "Engram — pinned dev toolchain (Rust + just + jq + sqlx-cli + cargo-watch + cargo-nextest).";

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
            cargo-nextest             # parallel test runner; `just check`
                                       # uses it for ~3-5× speedup over
                                       # `cargo test --workspace`.
            pkg-config
            openssl
            protobuf                # protoc, for tonic-build when grpc lands
            llvmPackages.libclang   # bindgen for userfaultfd-sys (Linux only,
                                    # but harmless on macOS)
          ] ++ lib.optionals stdenv.isDarwin [
            libiconv                # required by some macOS-aarch64 crates
          ];

          # OPENSSL_DIR / PKG_CONFIG_PATH so `cargo build` finds the
          # Nix-provided openssl instead of looking system-wide.
          # LIBCLANG_PATH points bindgen at the Nix-provided libclang;
          # without it, `userfaultfd-sys`'s build script can't find
          # libLLVM under the Nix loader.
          env = {
            OPENSSL_DIR = "${pkgs.openssl.dev}";
            OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
            PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          };

          shellHook = ''
            # Insulate cargo from the host's rustup installation.
            #
            # cargo searches for external subcommands (cargo-clippy,
            # cargo-fmt, …) in $CARGO_HOME/bin *before* PATH. If the
            # host has rustup, ~/.cargo/bin holds host-built plugins
            # that get preferred over the nix toolchain's, even
            # inside `nix develop`. On systems where the host glibc
            # is older than nix's (e.g. Ubuntu 22.04 host glibc 2.35
            # vs. nix glibc 2.42), those host plugins fail to dlopen
            # nix-built proc-macro .so files with `version GLIBC_2.X
            # not found`.
            #
            # Fix: point CARGO_HOME at a project-local directory so
            # bin/ is empty → cargo falls through to PATH and picks
            # nix's plugins. Symlink registry/ + git/ to the host's
            # ~/.cargo so we share the crates.io index and avoid a
            # cold re-download on first entry.
            export CARGO_HOME="$PWD/.nix/cargo"
            mkdir -p "$CARGO_HOME"
            if [ -d "$HOME/.cargo" ]; then
              for d in registry git; do
                if [ ! -e "$CARGO_HOME/$d" ] && [ -d "$HOME/.cargo/$d" ]; then
                  ln -s "$HOME/.cargo/$d" "$CARGO_HOME/$d"
                fi
              done
            fi
            echo "engram dev shell — rustc $(rustc --version | awk '{print $2}'), just $(just --version | awk '{print $2}'), fc=system, CARGO_HOME=$CARGO_HOME"
          '';
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}
