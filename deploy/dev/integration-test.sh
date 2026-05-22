#!/usr/bin/env bash
# ADR 0015 M5 smoke test: bake → enable → wait-for-host-ready →
# session-create against the local prod-shape stack. Run after
# `just integration-up`.
#
# Steps:
#   1. bake the demo image to the local registry (no canonical
#      memory capture — that machinery was retired in M5)
#   2. POST /api/enabled-images — should return ~immediately (the
#      cascade into templates is gone, the only work is pushing
#      disk chunks into BlobStorage)
#   3. poll GET /api/hosts until at least one host reports
#      `ready_images >= 1` (the prefetch supervisor needs to pull
#      the chunked rootfs from BlobStorage; sub-second on a warm
#      chunk cache, ~10-60s on a fresh cache depending on size)
#   4. POST /sessions — should succeed cold-create with the
#      chunks already local; sub-5s TTFM (down from ~17s with
#      on-demand GCS page-ins)
#   5. clean up
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

if ! curl -fsS "$COORD/healthz" >/dev/null 2>&1; then
    echo "ERROR: coord not reachable at $COORD" >&2
    echo "       Run 'just integration-up' first." >&2
    exit 1
fi

SHORT=$(git rev-parse --short HEAD)
LOCAL_REGISTRY="localhost:5001"
IMAGE_URI="$LOCAL_REGISTRY/integration-test/demo:warm-$SHORT"

echo "==> step 1/5: bake demo image"
echo "    target: $IMAGE_URI"
cargo build --release -p engram-cli >/dev/null 2>&1
cargo build --release --target x86_64-unknown-linux-musl \
    -p engram-agentd >/dev/null 2>&1

T0=$(date +%s.%N)
./target/release/engram-cli image build \
    --repo integration-test/demo \
    --tag "warm-$SHORT" \
    --source deploy/demo \
    --format ext4 \
    --images-dir ./var/integration/images \
    --inject-agent target/x86_64-unknown-linux-musl/release/engram-agentd \
    --push "$IMAGE_URI" \
    2>&1 | tail -3
T1=$(date +%s.%N)
echo "    bake+push elapsed: $(echo "$T1 - $T0" | bc)s"

echo ""
echo "==> step 2/5: POST /api/enabled-images"
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
echo "==> step 3/5: poll /api/hosts until THIS image's digest is ready"
# ADR 0015 M5: the host-agent's prefetch supervisor pulls chunks
# from BlobStorage in the background. We need to wait for THIS
# image's manifest_digest (each bake produces a unique one) to
# appear in some host's ready_image_digests, not just for the
# count to be >= 1 — a prior run's image may already be ready.
EXPECTED_DIGEST=$(curl -fsS "${AUTH_HEADER[@]}" "$COORD/api/enabled-images" \
    | python3 -c "import sys,json; rows = json.load(sys.stdin).get('images', []); print(next((r['manifest_digest'] for r in rows if r['image_uri']=='$IMAGE_URI'), ''))")
if [ -z "$EXPECTED_DIGEST" ]; then
    echo "ERROR: couldn't read manifest_digest from /api/enabled-images" >&2
    exit 1
fi
echo "    waiting for digest: $EXPECTED_DIGEST"
READY_DEADLINE=$(( $(date +%s) + 240 ))
T0=$(date +%s.%N)
while :; do
    READY=$(curl -fsS "${AUTH_HEADER[@]}" "$COORD/api/hosts" \
        | python3 -c "import sys,json; rows = json.load(sys.stdin).get('hosts', []); digests = {d for r in rows for d in r.get('ready_image_digests', [])}; print('yes' if '$EXPECTED_DIGEST' in digests else 'no')")
    if [ "$READY" = "yes" ]; then
        T1=$(date +%s.%N)
        READY_ELAPSED=$(echo "$T1 - $T0" | bc)
        echo "    host reported digest ready after ${READY_ELAPSED}s ✓"
        break
    fi
    if [ "$(date +%s)" -ge "$READY_DEADLINE" ]; then
        echo "ERROR: no host reported digest $EXPECTED_DIGEST ready within 240s" >&2
        echo "       host-agent prefetch may have stalled; check logs" >&2
        exit 1
    fi
    sleep 1
done

echo ""
echo "==> step 4/5: POST /sessions (cold-create with chunks already local)"
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

echo ""
echo "==> step 5/5: cleanup — DELETE /sessions/$SESS_ID"
curl -fsS -X DELETE "${AUTH_HEADER[@]}" \
    "$COORD/sessions/$SESS_ID" >/dev/null || true
echo "    session deleted"

trap - ERR
echo ""
echo "✓ integration smoke passed"
echo "  enable:       ${ENABLE_ELAPSED}s"
echo "  ready-wait:   ${READY_ELAPSED}s"
echo "  session:      ${SESS_ELAPSED}s ($SESS_KIND)"
