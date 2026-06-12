#!/usr/bin/env bash
# ADR 0015 M5 smoke test: bake → enable → wait-for-host-ready →
# session-create against the local prod-shape stack. Run after
# `just dev`.
#
# Steps:
#   1-3. bake + enable + ready-poll, factored out into
#        `integration-bake-demo.sh` so the CI e2e lane and this
#        smoke test share one bake path.
#   4. session create — should succeed cold-create with the
#      chunks already local; sub-5s TTFM (down from ~17s with
#      on-demand GCS page-ins).
#   5. clean up.
#
# On failure, dumps the tail of coord + host-agent logs.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
INTEG_DIR="./var/integration"
COORD="http://127.0.0.1:8090"

# gRPC address and bearer token for the app surface (ADR 0039).
export ENGRAM_APP_GRPC="${ENGRAM_APP_GRPC:-http://127.0.0.1:50061}"

CLI_TOKEN_FLAG=()
if [ -n "${ENGRAM_APP_TOKEN:-}" ]; then
    CLI_TOKEN_FLAG=(--token "$ENGRAM_APP_TOKEN")
fi

dump_logs() {
    echo ""
    echo "--- coord.log (last 60 lines) ---"
    tail -60 "$INTEG_DIR/coord.log" 2>/dev/null || true
    echo ""
    echo "--- host-agent.log (last 60 lines) ---"
    tail -60 "$INTEG_DIR/host-agent.log" 2>/dev/null || true
}
trap 'echo ""; echo "✗ FAILED — log tails above"; dump_logs' ERR

echo "==> steps 1-3/5: bake + enable + ready-poll"
IMAGE_URI=$(bash deploy/dev/integration-bake-demo.sh)
echo "    image enabled and ready: $IMAGE_URI"

echo ""
echo "==> step 4/5: session create (cold-create with chunks already local)"
T0=$(date +%s.%N)
# mode=dev_vm: harness=none equivalent (no harness driven)
SESS_ID=$(./target/release/engram-cli "${CLI_TOKEN_FLAG[@]}" \
    session create \
    --image "$IMAGE_URI" \
    --dev-vm)
T1=$(date +%s.%N)
SESS_ELAPSED=$(echo "$T1 - $T0" | bc)
echo "    session create elapsed: ${SESS_ELAPSED}s"
echo "    session_id: $SESS_ID"

echo ""
echo "==> step 5/5: cleanup — delete session $SESS_ID"
./target/release/engram-cli "${CLI_TOKEN_FLAG[@]}" \
    session delete "$SESS_ID"
echo "    session deleted"

trap - ERR
echo ""
echo "✓ integration smoke passed"
echo "  session:      ${SESS_ELAPSED}s (cold)"
echo "  (bake + enable + ready-wait timings printed by integration-bake-demo.sh above)"
