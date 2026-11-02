#!/usr/bin/env bash
# Real Linux wrappers and SDK transport. No VM, provider, or upstream server.
set -euo pipefail
[[ $(uname -s) == Linux ]] || { echo 'Linux wrapper gate requires Linux' >&2; exit 1; }
: "${CARGO_TARGET_DIR:?Set an owned absolute target directory}"
[[ "$CARGO_TARGET_DIR" == /* ]] || exit 1
# Keep generated Unix socket paths below Linux's sockaddr_un limit.
state=$(mktemp -d /tmp/mcp-linux.XXXXXXXX)
trap 'rm -rf "$state"' EXIT
cargo build --locked --manifest-path tests/fixtures/mcp-model-cli/Cargo.toml --bin engram-mcp-model-fixture
cargo build --locked -p engram-harness-claude -p engram-harness-codex --bins
export ENGRAM_FIXTURE_BIN="$CARGO_TARGET_DIR/debug/engram-mcp-model-fixture"
export ENGRAM_TEST_CLAUDE_BIN="$CARGO_TARGET_DIR/debug/engram-harness-claude"
export ENGRAM_TEST_CODEX_BIN="$CARGO_TARGET_DIR/debug/engram-harness-codex"
export ENGRAM_TEST_STATE_ROOT="$state"
for artifact in "$ENGRAM_FIXTURE_BIN" "$ENGRAM_TEST_CLAUDE_BIN" "$ENGRAM_TEST_CODEX_BIN"; do
  test -x "$artifact"
  file "$artifact" | grep -Fq 'ELF 64-bit'
done
test "$("$ENGRAM_FIXTURE_BIN" --fixture-version)" = 'engram-mcp-model-fixture 1'
timeout 240s cargo test --locked -p engram-harness-claude --test linux_mcp_entrypoint \
  -- --ignored --test-threads=1 2>&1 | tee "$state/entrypoint.log"
grep -Fq 'running 8 tests' "$state/entrypoint.log"
grep -Fq 'test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out' "$state/entrypoint.log"
