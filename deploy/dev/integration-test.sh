#!/usr/bin/env bash
# ADR 0015 M5 smoke test: bake → enable → wait-for-host-ready →
# session-create against the local prod-shape stack. Run after
# `just dev`.
#
# Steps:
#   1-3. bake + enable + ready-poll, factored out into
#        `integration-bake-demo.sh` so the CI e2e lane and this
#        smoke test share one bake path.
#   4. POST /sessions — should succeed cold-create with the
#      chunks already local; sub-5s TTFM (down from ~17s with
#      on-demand GCS page-ins).
#   5. clean up.
#
# On failure, dumps the tail of coord + host-agent logs.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
INTEG_DIR="./var/integration"
COORD="http://127.0.0.1:8090"

AUTH_HEADER=()
if [ -n "${ENGRAM_TOKEN:-}" ]; then
    AUTH_HEADER=(-H "Authorization: Bearer $ENGRAM_TOKEN")
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
echo "==> step 4/5: POST /sessions (cold-create with chunks already local)"
SESS_BODY=$(printf '{"image": "%s", "harness": {"kind": "none"}}' "$IMAGE_URI")
T0=$(date +%s.%N)
SESS_RESP=$(curl -fsS -X POST "${AUTH_HEADER[@]}" \
    -H "Content-Type: application/json" \
    -d "$SESS_BODY" \
    "$COORD/api/v1/sessions")
T1=$(date +%s.%N)
SESS_ELAPSED=$(echo "$T1 - $T0" | bc)
SESS_KIND=$(echo "$SESS_RESP" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("kind","?"))')
SESS_ID=$(echo "$SESS_RESP"  | python3 -c 'import sys,json; print(json.load(sys.stdin).get("session_id","?"))')
echo "    session create elapsed: ${SESS_ELAPSED}s"
echo "    session_id: $SESS_ID"
echo "    kind:       $SESS_KIND"

echo ""
echo "==> step 5/5: cleanup — DELETE /sessions/$SESS_ID"
curl -fsS -X DELETE "${AUTH_HEADER[@]}" \
    "$COORD/api/v1/sessions/$SESS_ID" >/dev/null || true
echo "    session deleted"

trap - ERR
echo ""
echo "✓ integration smoke passed"
echo "  session:      ${SESS_ELAPSED}s ($SESS_KIND)"
echo "  (bake + enable + ready-wait timings printed by integration-bake-demo.sh above)"
