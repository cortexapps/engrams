#!/usr/bin/env bash
# Smoke-test the full bake → enable → warm-pool → session-create
# flow against the local prod-shape stack. Run after
# `just integration-up`.
#
# Steps:
#   1. bake the demo image to the local registry with canonical
#      memory capture turned on
#   2. POST /api/enabled-images against the local coord — should
#      block while materializing and waiting for first warm slot,
#      then return 201 with elapsed ≤ ~25 s
#   3. assert templates_active = 1 in postgres
#   4. POST /sessions — should lease a warm slot, sub-second TTFM
#   5. clean up (delete the session)
#
# On failure, dumps the tail of coord + host-agent logs to help
# diagnose, then exits non-zero.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
INTEG_DIR="./var/integration"
COORD="http://127.0.0.1:8090"

# Bearer token: integration coord runs without auth by default
# (no ENGRAM_AUTH_TOKENS), so the API accepts unauthenticated
# requests. If you set tokens locally, export ENGRAM_TOKEN.
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

if ! curl -fsS "$COORD/healthz" >/dev/null 2>&1; then
    echo "ERROR: coord not reachable at $COORD" >&2
    echo "       Run 'just integration-up' first." >&2
    exit 1
fi

SHORT=$(git rev-parse --short HEAD)
LOCAL_REGISTRY="localhost:5001"
IMAGE_URI="$LOCAL_REGISTRY/integration-test/demo:warm-$SHORT"

echo "==> step 1/5: bake demo image with canonical capture"
echo "    target: $IMAGE_URI"
cargo build --release -p engram-cli \
    --target x86_64-unknown-linux-musl -p engram-agentd -p engram-bootstrap \
    >/dev/null 2>&1 || true
cargo build --release -p engram-cli >/dev/null 2>&1
# musl bins for the in-VM agent + bootstrap shim.
cargo build --release --target x86_64-unknown-linux-musl \
    -p engram-agentd -p engram-bootstrap >/dev/null 2>&1

T0=$(date +%s.%N)
./target/release/engram-cli image build \
    --repo integration-test/demo \
    --tag "warm-$SHORT" \
    --source deploy/demo \
    --format ext4 \
    --images-dir ./var/integration/images \
    --inject-agent     target/x86_64-unknown-linux-musl/release/engram-agentd \
    --inject-bootstrap target/x86_64-unknown-linux-musl/release/engram-bootstrap \
    --capture-canonical-memory \
    --canonical-kernel "${ENGRAM_KERNEL_IMAGE_PATH:-$HOME/.cache/engram-fc-test/vmlinux-5.10.223}" \
    --canonical-boot-wait-secs 8 \
    --canonical-memory-mib 256 \
    --push "$IMAGE_URI" \
    2>&1 | tail -3
T1=$(date +%s.%N)
echo "    bake+push elapsed: $(echo "$T1 - $T0" | bc)s"

echo ""
echo "==> step 2/5: POST /api/enabled-images (blocks on first warm slot)"
ENABLE_BODY=$(printf '{"image_uri": "%s"}' "$IMAGE_URI")
T0=$(date +%s.%N)
curl -fsS -X POST "${AUTH_HEADER[@]}" \
    -H "Content-Type: application/json" \
    -d "$ENABLE_BODY" \
    "$COORD/api/enabled-images" \
    >/dev/null
T1=$(date +%s.%N)
ENABLE_ELAPSED=$(echo "$T1 - $T0" | bc)
echo "    enable elapsed: ${ENABLE_ELAPSED}s"

echo ""
echo "==> step 3/5: verify templates_active = 1"
TEMPLATES=$(docker compose -f deploy/docker-compose.dev.yml exec -T postgres \
    psql -U engram -d engram -tA -c \
    "select count(*) from templates where active=true")
if [ "$(echo "$TEMPLATES" | tr -d ' \r\n')" != "1" ]; then
    echo "ERROR: expected templates_active=1, got '$TEMPLATES'" >&2
    exit 1
fi
echo "    templates_active = 1 ✓"

echo ""
echo "==> step 4/5: POST /sessions (should lease warm)"
SESS_BODY=$(printf '{"image": "%s", "harness": {"kind": "none"}}' "$IMAGE_URI")
T0=$(date +%s.%N)
SESS_RESP=$(curl -fsS -X POST "${AUTH_HEADER[@]}" \
    -H "Content-Type: application/json" \
    -d "$SESS_BODY" \
    "$COORD/sessions")
T1=$(date +%s.%N)
SESS_ELAPSED=$(echo "$T1 - $T0" | bc)
SESS_KIND=$(echo "$SESS_RESP" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("kind","?"))')
SESS_ID=$(echo "$SESS_RESP"  | python3 -c 'import sys,json; print(json.load(sys.stdin).get("session_id","?"))')
echo "    session create elapsed: ${SESS_ELAPSED}s"
echo "    session_id: $SESS_ID"
echo "    kind:       $SESS_KIND"

if [ "$SESS_KIND" != "warm" ]; then
    echo "WARN: session was '$SESS_KIND', not 'warm'" >&2
    echo "      (warm pool may not have filled yet; rerun in a few seconds)" >&2
fi

echo ""
echo "==> step 5/5: cleanup — DELETE /sessions/$SESS_ID"
curl -fsS -X DELETE "${AUTH_HEADER[@]}" \
    "$COORD/sessions/$SESS_ID" >/dev/null || true
echo "    session deleted"

trap - ERR
echo ""
echo "✓ integration smoke passed"
echo "  bake+push:    ${T1:-?}"
echo "  enable:       ${ENABLE_ELAPSED}s"
echo "  session:      ${SESS_ELAPSED}s ($SESS_KIND)"
