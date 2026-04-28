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

# The real-VM exec test bakes engram-agentd into the rootfs. Build it
# statically for `x86_64-unknown-linux-musl` so the binary runs inside
# any rootfs without depending on the Nix dev shell's glibc /
# dynamic-linker paths. Release mode keeps the bin small.
cargo build -p engram-agentd \
  --target x86_64-unknown-linux-musl \
  --release

case "${1:-all}" in
  boot|lifecycle|snapshot|snapshot_uffd|exec_real_vm)
    exec cargo test -p engram-sandbox-firecracker --test "$1" -- --ignored --nocapture
    ;;
  all)
    cargo test -p engram-sandbox-firecracker --test boot           -- --ignored --nocapture
    cargo test -p engram-sandbox-firecracker --test lifecycle      -- --ignored --nocapture
    cargo test -p engram-sandbox-firecracker --test snapshot       -- --ignored --nocapture
    cargo test -p engram-sandbox-firecracker --test snapshot_uffd  -- --ignored --nocapture
    cargo test -p engram-sandbox-firecracker --test exec_real_vm   -- --ignored --nocapture
    ;;
  *)
    echo "usage: $0 [boot|lifecycle|snapshot|snapshot_uffd|exec_real_vm|all]" >&2
    exit 2
    ;;
esac
