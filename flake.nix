{
  description = "Engram — pinned dev toolchain (Rust + just + jq + sqlx-cli + cargo-watch + cargo-nextest).";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # ADR 0036: byte-deterministic ext4 packs (engram-image-builder) need an
    # e2fsprogs that honors SOURCE_DATE_EPOCH (added in 1.47.1) — apt on the
    # CI/bake runners ships 1.47.0. But 1.47.3 introduced a `mke2fs -d`
    # regression that fails on any file > 2 GiB ("Ext2 file too big"), tripped by
    # dev-brain's 2.3 GB compose-images.tar. So pin a nixpkgs rev that ships
    # e2fsprogs EXACTLY 1.47.2 (SOURCE_DATE_EPOCH, no >2 GiB bug) — and one from
    # *before* nixpkgs added the libarchive feature (2025-06-28), which breaks
    # the static-musl link. This is the single pinned source of a reproducible
    # mke2fs — for `nix develop`, the determinism test, and the image bakes.
    nixpkgs-e2fsprogs.url = "github:NixOS/nixpkgs/c6729488de632cc9f8af5aa82edd7097ea506c36";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, nixpkgs-e2fsprogs, flake-utils, rust-overlay, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        # ADR 0036: e2fsprogs 1.47.2 (pinned via the nixpkgs-e2fsprogs input rev,
        # see the input note) — SOURCE_DATE_EPOCH determinism without the 1.47.3
        # >2 GiB `mke2fs -d` regression.
        e2fsprogsPkgs = import nixpkgs-e2fsprogs { inherit system; };
        e2fsprogs = e2fsprogsPkgs.e2fsprogs;

        # ADR 0036: a STATIC (musl) mke2fs to BUNDLE into the `cli-tools`
        # artifact, so `engram-cli image build` runs a pinned, portable mke2fs
        # sibling on any runner — no nix store, no PATH dependency. The dynamic
        # `e2fsprogs` above is fine for `nix develop` and the packer's CI test
        # (its lib closure is present), but a binary copied into an OCI artifact
        # must be self-contained. Linux-only (pkgsStatic targets musl); the
        # `.bin` output carries `sbin/mke2fs`. Same 1.47.2 pin (input rev).
        e2fsprogsStatic = e2fsprogsPkgs.pkgsStatic.e2fsprogs;

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
            tilt                    # `just dev` orchestrator (ADR 0024).
                                    # One engine on every host: macOS via
                                    # nix develop or brew, the Linux dev-vm
                                    # via nix develop. Replaces the bespoke
                                    # integration-up.sh path.
            jq
            postgresql              # psql + pg_dump; not the server
            sqlx-cli
            cargo-watch
            cargo-nextest             # parallel test runner; `just check`
                                       # uses it for ~3-5× speedup over
                                       # `cargo test --workspace`.
            cargo-hakari              # workspace-hack feature-unifier.
                                       # See `workspace-hack/` and
                                       # `.config/hakari.toml`. `cargo
                                       # hakari verify` runs in CI to
                                       # keep the union current.
            pkg-config
            openssl
            protobuf                # protoc, for tonic-build when grpc lands
            llvmPackages.libclang   # bindgen for userfaultfd-sys (Linux only,
                                    # but harmless on macOS)
            nodejs_22               # web SPA dev server (`just web` -> vite)
            pnpm                    # workspace package manager for web/
            # ADR 0036: a SOURCE_DATE_EPOCH-honoring mke2fs (e2fsprogs >= 1.47.1)
            # for byte-deterministic ext4 packs — `just bake-demo`, the
            # determinism test, and `engram-cli image build`. The `e2fsprogs`
            # let-binding (pinned current nixpkgs) lexically shadows the stale
            # `pkgs.e2fsprogs` here. All platforms (was macOS-only).
            e2fsprogs
            # ADR 0055 P2: mksquashfs packs an uploaded skill dir into a
            # content-addressed RO squashfs at registration (the coordinator's
            # `skill_pack`); also what the deploy/bundles/* recipes already use.
            # Needed by `nix develop` (the coordinator + `just check`'s pack test)
            # and bundled into the coordinator runtime image.
            squashfsTools
          ] ++ lib.optionals stdenv.isLinux [
            # Parallel linker; wired in via the `shellHook` below
            # (CARGO_TARGET_*_UNKNOWN_LINUX_GNU_RUSTFLAGS). Cuts
            # incremental link time on this multi-binary workspace
            # by 3–10× vs. GNU ld — engram ships ~10 binaries
            # (coord/host-agent/agentd/uffd-handler/cli/...) so the
            # savings compound on every edit-rebuild. macOS is not
            # wired: Apple's ld is already fast and lld's Mach-O
            # support is fragile.
            mold
          ] ++ lib.optionals stdenv.isDarwin [
            libiconv                # required by some macOS-aarch64 crates
            # `just bake-demo` cross-compiles the in-guest musl binaries
            # (agentd, harness-claude); the cross stdenvs ship `<target>-cc`
            # linkers wired below in the shellHook (.cargo/config.toml hard-codes
            # the brew binary names, so we override via CARGO_TARGET_*_LINKER).
            # (mke2fs for the ext4 pack is now common, above — no brew needed.)
            pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc
            pkgs.pkgsCross.musl64.stdenv.cc
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

            # Linker selection for native Linux targets: tell rustc
            # to pass `-fuse-ld=mold` to the C-compiler driver. gcc
            # ≥12 (Ubuntu 22.04+) and clang ≥12 both honor this, and
            # mold itself comes from the nix shell above. macOS skips
            # this entirely — Apple's `ld` is fast and lld's Mach-O
            # support is fragile. CI runners install mold via the
            # `rui314/setup-mold` action; this hook only wires up the
            # `nix develop` shell.
            if [ "$(uname -s)" = "Linux" ]; then
              export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-fuse-ld=mold"
              export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="-C link-arg=-fuse-ld=mold"
            fi

            # macOS cross-compile to the musl guest targets (`just bake-demo`).
            # `.cargo/config.toml` names the brew linkers (aarch64-linux-musl-gcc);
            # point cargo + the `cc` crate (ring's C/asm) at the nix cross
            # toolchains instead, so no Homebrew is needed. Env overrides
            # `.cargo/config.toml`'s `linker=`; the `relocation-model=static`
            # rustflags there still apply.
            if [ "$(uname -s)" = "Darwin" ]; then
              ARM64_CC="${pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc}/bin/aarch64-unknown-linux-musl-cc"
              X86_CC="${pkgs.pkgsCross.musl64.stdenv.cc}/bin/x86_64-unknown-linux-musl-cc"
              export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$ARM64_CC"
              export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="$X86_CC"
              export CC_aarch64_unknown_linux_musl="$ARM64_CC"
              export CC_x86_64_unknown_linux_musl="$X86_CC"
              # The nix cross stdenvs above export a GENERIC `CC`/`CXX`
              # (one of the musl gccs). cc-rs falls back to that generic
              # CC for HOST (apple-darwin) build-dependency C — e.g. ring's
              # curve25519.c built for a proc-macro — and pairs it with
              # macOS flags (`-arch arm64 -mmacosx-version-min`), so a musl
              # gcc gets `-arch arm64` and the build dies. Pin the host
              # target's compiler back to the nix clang wrapper so host C
              # builds use clang while only the *-linux-musl targets use the
              # cross gccs. Without this, `cargo {check,clippy} --target
              # aarch64-unknown-linux-musl` (the local Linux cross-check)
              # fails on ring; `just bake-demo` happened to dodge it by not
              # pulling a host-built ring.
              export CC_aarch64_apple_darwin="$(command -v cc)"
              export CXX_aarch64_apple_darwin="$(command -v c++)"
              export CC_x86_64_apple_darwin="$(command -v cc)"
              export CXX_x86_64_apple_darwin="$(command -v c++)"
              # The musl cross sysroots ship libc but NOT the Linux kernel
              # UAPI headers, so a kernel-header bindgen crate (userfaultfd-sys,
              # and the C `cc` step) can't find <linux/userfaultfd.h> when you
              # cross-check the Linux-gated crates from macOS. Point clang
              # (bindgen) and the `cc` crate at the cross targets' kernel
              # headers. This is what makes `cargo {check,clippy} --target
              # aarch64-unknown-linux-musl` work in `nix develop` for
              # engram-{host-agent,uffd-handler,sandbox-firecracker}.
              export BINDGEN_EXTRA_CLANG_ARGS="''${BINDGEN_EXTRA_CLANG_ARGS:+$BINDGEN_EXTRA_CLANG_ARGS }-I${pkgs.pkgsCross.aarch64-multiplatform-musl.linuxHeaders}/include"
              export CFLAGS_aarch64_unknown_linux_musl="-I${pkgs.pkgsCross.aarch64-multiplatform-musl.linuxHeaders}/include"
              export CFLAGS_x86_64_unknown_linux_musl="-I${pkgs.pkgsCross.musl64.linuxHeaders}/include"
            fi
          '';
        };

        # ADR 0036: the reproducible mke2fs as buildable outputs, one pinned rev
        # for everything.
        #   .#mke2fs        — dynamic; for `nix develop` + the packer's CI test
        #                     (`nix build .#mke2fs` → bin/mke2fs, lib closure in
        #                     the nix store). `.bin` is e2fsprogs' bin output.
        #   .#mke2fs-static — static musl; copied into the `cli-tools` artifact
        #                     so engram-cli runs it as a portable sibling on any
        #                     runner (`sbin/mke2fs`). Linux-only (pkgsStatic).
        packages = {
          mke2fs = e2fsprogs.bin;
        } // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          mke2fs-static = e2fsprogsStatic.bin;
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}
