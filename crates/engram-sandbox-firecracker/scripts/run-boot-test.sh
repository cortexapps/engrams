#!/usr/bin/env bash
# Convenience wrapper: fetch artifacts (cached) and run the Firecracker
# integration tests. Meant to run on the Linux dev VM inside `nix
# develop`. Pass a specific test binary as $1 (`boot` or `lifecycle`)
# or run both with no arg.
set -euo pipefail
here="$(dirname "${BASH_SOURCE[0]}")"

# fetch-fc-test-artifacts.sh prints `export FC_TEST_KERNEL=...` lines.
eval "$(bash "$here/fetch-fc-test-artifacts.sh")"

case "${1:-all}" in
  boot|lifecycle)
    exec cargo test -p engram-sandbox-firecracker --test "$1" -- --ignored --nocapture
    ;;
  all)
    cargo test -p engram-sandbox-firecracker --test boot      -- --ignored --nocapture
    cargo test -p engram-sandbox-firecracker --test lifecycle -- --ignored --nocapture
    ;;
  *)
    echo "usage: $0 [boot|lifecycle|all]" >&2
    exit 2
    ;;
esac
