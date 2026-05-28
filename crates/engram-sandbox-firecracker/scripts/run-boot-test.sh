#!/usr/bin/env bash
# Convenience wrapper: fetch artifacts (cached) and run the Firecracker
# integration tests. Meant to run on the Linux dev VM inside `nix
# develop`. Pass a specific test binary as $1 (`boot` or `lifecycle`)
# or run both with no arg.
set -euo pipefail
here="$(dirname "${BASH_SOURCE[0]}")"

# fetch-fc-test-artifacts.sh prints `export FC_TEST_KERNEL=...` lines.
eval "$(bash "$here/fetch-fc-test-artifacts.sh")"

# UFFD restore needs the handler binary built first.
cargo build -p engram-uffd-handler

# The real-VM tests bake engram-agentd and engram-harness-noop into
# the rootfs / harness substrate. Build them statically for
# `x86_64-unknown-linux-musl` so they run inside any rootfs without
# depending on the Nix dev shell's glibc / dynamic-linker paths.
# .cargo/config.toml's `relocation-model=static` ensures the
# resulting ELFs have no PT_INTERP. Release mode keeps binaries
# small.
cargo build \
  -p engram-agentd \
  -p engram-harness-noop \
  --target x86_64-unknown-linux-musl \
  --release

case "${1:-all}" in
  boot|lifecycle|snapshot|snapshot_uffd|exec_real_vm|baked_harness_loopback|multi_restore|cross_host_restore|snapshot_net|host_startup)
    # snapshot_net + host_startup need root for TAP/iptables.
    # Detect and re-exec via sudo when not already root.
    if [ "$1" = "host_startup" ] || [ "$1" = "snapshot_net" ]; then
      if [ "$(id -u)" -ne 0 ]; then
        exec sudo -E env "PATH=$PATH" cargo test -p engram-sandbox-firecracker --test "$1" -- --ignored --nocapture --test-threads=1
      fi
    fi
    exec cargo test -p engram-sandbox-firecracker --test "$1" -- --ignored --nocapture
    ;;
  all)
    # Mirrors ci.yml's unprivileged FC test list.
    cargo test -p engram-sandbox-firecracker --test boot                   -- --ignored --nocapture
    cargo test -p engram-sandbox-firecracker --test lifecycle              -- --ignored --nocapture
    cargo test -p engram-sandbox-firecracker --test snapshot               -- --ignored --nocapture
    cargo test -p engram-sandbox-firecracker --test snapshot_uffd          -- --ignored --nocapture
    cargo test -p engram-sandbox-firecracker --test exec_real_vm           -- --ignored --nocapture
    cargo test -p engram-sandbox-firecracker --test baked_harness_loopback -- --ignored --nocapture
    cargo test -p engram-sandbox-firecracker --test multi_restore          -- --ignored --nocapture
    cargo test -p engram-sandbox-firecracker --test cross_host_restore     -- --ignored --nocapture
    ;;
  *)
    echo "usage: $0 [boot|lifecycle|snapshot|snapshot_uffd|exec_real_vm|baked_harness_loopback|multi_restore|cross_host_restore|snapshot_net|host_startup|all]" >&2
    exit 2
    ;;
esac
