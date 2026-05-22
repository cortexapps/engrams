#!/usr/bin/env bash
# Persistent dev session — bakes + enables + creates a session and
# leaves it running so you can poke at it with curl, wscat, or the
# web UI. Companion to `integration-test.sh` (which always deletes
# at the end).
#
# Idempotent across iterations:
#   • If the demo image at the current HEAD is already enabled,
#     skip the bake/enable.
#   • If a session from a previous run is still active, print its
#     id and exit — don't keep stacking.
#
# Common invocations:
#   bash deploy/dev/integration-session.sh            # default
#   HARNESS=claude bash deploy/dev/integration-session.sh
#   PROMPT='hi' HARNESS=claude bash deploy/dev/integration-session.sh
#
# Cleanup: `just integration-down` reaps the session along with
# everything else; or curl -X DELETE $COORD/sessions/$SID directly.

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
INTEG_DIR="./var/integration"
COORD="http://127.0.0.1:8090"
HARNESS="${HARNESS:-none}"
PROMPT="${PROMPT:-}"

AUTH_HEADER=()
if [ -n "${ENGRAM_TOKEN:-}" ]; then
    AUTH_HEADER=(-H "Authorization: Bearer $ENGRAM_TOKEN")
fi

if ! curl -fsS "$COORD/healthz" >/dev/null 2>&1; then
    echo "ERROR: coord not reachable at $COORD" >&2
    echo "       Run 'just integration-up' first." >&2
    exit 1
fi

SHORT=$(git rev-parse --short HEAD)
LOCAL_REGISTRY="localhost:5001"
IMAGE_URI="$LOCAL_REGISTRY/integration-test/demo:warm-$SHORT"

# Already enabled?
already_enabled=$(curl -fsS "${AUTH_HEADER[@]}" "$COORD/api/enabled-images" \
    | python3 -c "import sys,json; d=json.load(sys.stdin); print(any(i.get('image_uri')=='$IMAGE_URI' for i in d.get('images',[])))" 2>/dev/null || echo False)

if [ "$already_enabled" = "True" ]; then
    echo "==> image $IMAGE_URI already enabled; skipping bake"
else
    echo "==> baking demo image @ $SHORT"
    cargo build --release -p engram-cli >/dev/null 2>&1
    cargo build --release --target x86_64-unknown-linux-musl \
        -p engram-agentd >/dev/null 2>&1
    ./target/release/engram-cli image build \
        --repo integration-test/demo \
        --tag "warm-$SHORT" \
        --source deploy/demo \
        --format ext4 \
        --images-dir ./var/integration/images \
        --inject-agent target/x86_64-unknown-linux-musl/release/engram-agentd \
        --capture-canonical-memory \
        --canonical-kernel "${ENGRAM_KERNEL_IMAGE_PATH:-$HOME/.cache/engram-fc-test/vmlinux-5.10.223}" \
        --canonical-boot-wait-secs 8 \
        --canonical-memory-mib 256 \
        --push "$IMAGE_URI" \
        2>&1 | tail -3

    echo "==> POST /api/enabled-images (cascade)"
    ENABLE_BODY=$(printf '{"image_uri": "%s"}' "$IMAGE_URI")
    curl -fsS -X POST "${AUTH_HEADER[@]}" \
        -H "Content-Type: application/json" \
        -d "$ENABLE_BODY" \
        "$COORD/api/enabled-images" \
        >/dev/null
fi

# Look for an existing dev-session row (status=active, this image,
# tagged via metadata-style harness so we can find it). The session
# row doesn't carry a label field, so we filter by image+status and
# pick the most recent. Best-effort: stale rows from prior runs are
# possible if integration-down didn't reap.
existing_sid=$(curl -fsS "${AUTH_HEADER[@]}" "$COORD/sessions" 2>/dev/null \
    | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
except Exception:
    d = {}
for s in d.get('sessions', []):
    if s.get('status') == 'active' and s.get('image','').endswith('warm-$SHORT'):
        print(s['id']); break
" || true)

if [ -n "$existing_sid" ]; then
    SID="$existing_sid"
    echo "==> reusing existing active session $SID"
else
    echo "==> POST /sessions  (harness=$HARNESS)"
    if [ -n "$PROMPT" ]; then
        SESS_BODY=$(python3 -c "
import json
print(json.dumps({
    'image': '$IMAGE_URI',
    'harness': {'kind': 'builtin', 'name': '$HARNESS'} if '$HARNESS' != 'none' else {'kind': 'none'},
    'prompt': '$PROMPT',
    'secrets': {'ANTHROPIC_API_KEY': 'sk-bogus-dev-session'},
}))
")
    else
        SESS_BODY=$(python3 -c "
import json
print(json.dumps({
    'image': '$IMAGE_URI',
    'harness': {'kind': 'builtin', 'name': '$HARNESS'} if '$HARNESS' != 'none' else {'kind': 'none'},
}))
")
    fi
    SESS_RESP=$(curl -fsS -X POST "${AUTH_HEADER[@]}" \
        -H "Content-Type: application/json" \
        -d "$SESS_BODY" \
        "$COORD/sessions")
    SID=$(echo "$SESS_RESP" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("session_id","?"))')
    KIND=$(echo "$SESS_RESP" | python3 -c 'import sys,json; print(json.load(sys.stdin).get("kind","?"))')
    echo "    session_id=$SID  kind=$KIND"
fi

echo ""
echo "✓ session $SID is live — iterate:"
echo ""
echo "  # exec a one-shot command"
echo "  curl -s -XPOST $COORD/sessions/$SID/exec \\"
echo "    -H 'Content-Type: application/json' \\"
echo "    -d '{\"command\":\"ls /\"}' | jq ."
echo ""
echo "  # open the shell (browser, after port-forwarding)"
echo "  open http://localhost:5173/sessions/$SID"
echo ""
echo "  # raw events"
echo "  curl -s $COORD/sessions/$SID/events | tail"
echo ""
echo "  # tear down"
echo "  curl -s -XDELETE $COORD/sessions/$SID"
