#!/usr/bin/env bash
# Real Linux wrappers and SDK transport. No VM, provider, or upstream server.
set -euo pipefail
[[ $(uname -s) == Linux ]] || { echo 'Linux wrapper gate requires Linux' >&2; exit 1; }
: "${CARGO_TARGET_DIR:?Set an owned absolute target directory}"
[[ "$CARGO_TARGET_DIR" == /* ]] || exit 1
cargo build --locked --manifest-path tests/fixtures/mcp-model-cli/Cargo.toml --bin engram-mcp-model-fixture
cargo build --locked -p engram-harness-claude -p engram-harness-codex --bins
export ENGRAM_FIXTURE_BIN="$CARGO_TARGET_DIR/debug/engram-mcp-model-fixture"
export ENGRAM_TEST_CLAUDE_BIN="$CARGO_TARGET_DIR/debug/engram-harness-claude"
export ENGRAM_TEST_CODEX_BIN="$CARGO_TARGET_DIR/debug/engram-harness-codex"
for artifact in "$ENGRAM_FIXTURE_BIN" "$ENGRAM_TEST_CLAUDE_BIN" "$ENGRAM_TEST_CODEX_BIN"; do
  test -x "$artifact"
  file "$artifact" | grep -Fq 'ELF 64-bit'
done
test "$("$ENGRAM_FIXTURE_BIN" --fixture-version)" = 'engram-mcp-model-fixture 1'
# An example is an explicitly built test utility, never a production binary
# or a harness=false target that breaks workspace nextest discovery.
cargo build --locked -p engram-harness-claude --example linux_mcp_supervisor
# Keep generated Unix socket paths below Linux's sockaddr_un limit.
state=$(mktemp -d /tmp/mcp-linux.XXXXXXXX)
trap 'if [[ -f "$state/cleanup-complete" ]]; then rm -rf "$state"; else echo "Cleanup unconfirmed; retained owned state: $state" >&2; fi' EXIT
export ENGRAM_TEST_STATE_ROOT="$state"
cargo test --locked -p engram-harness-claude --test linux_mcp_entrypoint \
  --no-run --message-format=json > "$state/artifacts.jsonl"
export ENGRAM_TEST_DRIVER_BIN
ENGRAM_TEST_DRIVER_BIN=$(python3 -c '
import json, sys
paths = [m["executable"] for line in sys.stdin if (m := json.loads(line)).get("reason") == "compiler-artifact" and m.get("executable") and m["target"]["name"] == "linux_mcp_entrypoint"]
assert len(paths) == 1, "one current Linux test driver is required"
print(paths[0])
' < "$state/artifacts.jsonl")
test -x "$ENGRAM_TEST_DRIVER_BIN"
# The supervisor has an earlier bounded deadline and reaps all descendants.
# This hard outer kill is only a final bound if the supervisor itself fails.
timeout --signal=KILL 240s "$CARGO_TARGET_DIR/debug/examples/linux_mcp_supervisor" \
  --check-cleanup-and-run 2>&1 | tee "$state/entrypoint.log"
grep -Fq 'forced deadline cleanup: passed; all descendants reaped before state removal' "$state/entrypoint.log"
grep -Fq 'running 8 tests' "$state/entrypoint.log"
grep -Fq 'test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out' "$state/entrypoint.log"
test "$(cat "$state/cleanup-complete")" = complete
