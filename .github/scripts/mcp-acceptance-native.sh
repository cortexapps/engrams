#!/usr/bin/env bash
set -euo pipefail
: "${CARGO_TARGET_DIR:?Set an owned target directory}"
root="$PWD"
mkdir -p var/mcp-acceptance-ci
state=$(mktemp -d "$root/var/mcp-acceptance-ci/native.XXXXXXXX")
trap 'rm -rf "$state"' EXIT
unset ENGRAM_FIXTURE_BARRIER
cargo test --locked --manifest-path tests/fixtures/mcp-model-cli/Cargo.toml --bins --test protocol 2>&1 | tee "$state/protocol.log"
# Binary unit results cannot satisfy the protocol suite's positive-count guard.
awk '
  /Running tests\/protocol.rs / { protocol = 1; next }
  protocol && /^test result: ok\. [1-9][0-9]* passed; 0 failed; 0 ignored/ { passed = 1 }
  END { exit !passed }
' "$state/protocol.log"
grep -Fxq 'test evidence::tests::expired_deadline_cannot_consume_a_release ... ok' "$state/protocol.log"
cargo build --locked --manifest-path tests/fixtures/mcp-model-cli/Cargo.toml --bin engram-mcp-model-fixture
export ENGRAM_FIXTURE_BIN="$CARGO_TARGET_DIR/debug/engram-mcp-model-fixture"
test -x "$ENGRAM_FIXTURE_BIN"
test "$("$ENGRAM_FIXTURE_BIN" --fixture-version)" = 'engram-mcp-model-fixture 1'
export ENGRAM_FIXTURE_TOOL=remote_inventory
for mode in read held-call; do
  export ENGRAM_FIXTURE_MODE="$mode"
  export ENGRAM_FIXTURE_STATE_DIR="$state/$mode"
  mkdir -m 700 "$ENGRAM_FIXTURE_STATE_DIR"
  export ENGRAM_FIXTURE_PROOF="$ENGRAM_FIXTURE_STATE_DIR/proof"
  cargo test --locked -p engram-harness-codex --bin engram-harness-codex \
    tests::external_mcp_static_fixture_adapter_gate -- --ignored --exact 2>&1 | tee "$state/$mode.log"
  grep -Fq 'test result: ok. 1 passed; 0 failed; 0 ignored' "$state/$mode.log"
done
