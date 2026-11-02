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
  -E 'test(=managed::helper::tests::actual_helper_closed_lifetime_and_descriptor_isolation) or test(=managed::helper::tests::actual_helper_release_ack_and_active_dns_cleanup) or test(=managed::helper::tests::actual_helper_rejects_wrong_epoch_and_canceled_release)' \
  --no-tests fail --test-threads 1 2>&1 | tee "$log"
grep -Eq "(^|[[:space:]])3 tests run: 3 passed, [0-9]+ skipped([[:space:]]|$)" "$log"
