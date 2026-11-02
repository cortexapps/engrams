#!/usr/bin/env bash
set -euo pipefail
root="$PWD"
artifact="$root/var/mcp-netstack-artifact"
(cd "$artifact" && test -s engram-sandbox-netstack && shasum -a 256 -c SHA256SUMS)
export ENGRAM_SANDBOX_NETSTACK_TEST_BIN="$artifact/engram-sandbox-netstack"
test -x "$ENGRAM_SANDBOX_NETSTACK_TEST_BIN"
mkdir -p var/mcp-acceptance-ci
log=$(mktemp "$root/var/mcp-acceptance-ci/helper.XXXXXXXX")
trap 'rm -f "$log"' EXIT
cargo nextest run -p engram-sandbox-vz --lib --run-ignored ignored-only \
  -E 'test(=managed::helper::tests::actual_helper_closed_lifetime_and_descriptor_isolation)' \
  --no-tests fail 2>&1 | tee "$log"
grep -Eq "1 test run: 1 passed, [0-9]+ skipped" "$log"
