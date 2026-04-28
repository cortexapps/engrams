#!/usr/bin/env bash
# Convenience wrapper: fetch artifacts (cached) and run the boot smoke
# test. Meant to run on the Linux dev VM inside `nix develop`.
set -euo pipefail
here="$(dirname "${BASH_SOURCE[0]}")"

# fetch-fc-test-artifacts.sh prints `export FC_TEST_KERNEL=...` lines.
eval "$(bash "$here/fetch-fc-test-artifacts.sh")"

exec cargo test -p engram-sandbox-firecracker --test boot -- --ignored --nocapture
