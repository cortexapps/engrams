#!/usr/bin/env bash
# Persistent dev session — builds + enables the demo image (ADR 0080:
# plain `docker build && docker push`, via integration-bake-demo.sh)
# + creates a session and leaves it running so you can poke at it with
# curl, wscat, or the web UI. Companion to `integration-test.sh`
# (which always deletes at the end).
#
# Idempotent across iterations:
#   • If the demo image at the current HEAD is already enabled,
#     skip the build/enable.
#   • If a session from a previous run is still active, print its
#     id and exit — don't keep stacking.
#
# Common invocations:
#   bash deploy/dev/integration-session.sh            # default
#   HARNESS=claude bash deploy/dev/integration-session.sh
#   PROMPT='hi' HARNESS=claude bash deploy/dev/integration-session.sh
#
# Cleanup: `just dev-down` reaps the session along with everything
# else; or `engrams session delete $SID` directly.

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

COORD="http://127.0.0.1:8090"
HARNESS="${HARNESS:-none}"
PROMPT="${PROMPT:-}"

# Image + session verbs go through the `engrams` CLI → the orchestrator's
# Connect surface (the coordinator app-gRPC is internal). Endpoint + admin
# key default to the `just dev` stack (Tilt seeds var/dev-api-key). (The
# coord `/healthz` check below is a KEPT internal REST route, left as-is.)
source deploy/dev/engrams-cli.sh

if ! curl -fsS "$COORD/healthz" >/dev/null 2>&1; then
    echo "ERROR: coord not reachable at $COORD" >&2
    echo "       Run 'just dev' first." >&2
    exit 1
fi

SHORT=$(git rev-parse --short HEAD)
LOCAL_REGISTRY="localhost:5001"
IMAGE_URI="$LOCAL_REGISTRY/integration-test/demo:warm-$SHORT"

# Already enabled?
already_enabled=$(engrams --json image list 2>/dev/null \
    | python3 -c "import sys,json; d=json.load(sys.stdin); print(any(i.get('image_uri')=='$IMAGE_URI' for i in d.get('images',[])))" 2>/dev/null || echo False)

if [ "$already_enabled" = "True" ]; then
    echo "==> image $IMAGE_URI already enabled; skipping build+enable"
else
    # ADR 0080 phase 3b: one build+enable choreography — docker build +
    # push a PLAIN docker image, enable (host-side materialize +
    # capture), and wait for digest-ready. Shared with the CI e2e lane;
    # prints the same $IMAGE_URI computed above.
    echo "==> build + enable demo image @ $SHORT (via integration-bake-demo.sh)"
    IMAGE_URI=$(bash deploy/dev/integration-bake-demo.sh)
    echo "    enabled $IMAGE_URI"
fi

# Look for an existing dev-session row (status=active, this image,
# tagged via metadata-style harness so we can find it). The session
# row doesn't carry a label field, so we filter by image+status and
# pick the most recent. Best-effort: stale rows from prior runs are
# possible if `just dev-down` didn't reap.
existing_sid=$(engrams --json session list 2>/dev/null \
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
    # ADR 0051 + ADR 0021 P1.3: which harness an image runs is now an
    # image property, not a per-session choice. `session create` drives
    # the image's baked harness by default; `--dev-vm` boots the image as
    # a pure dev VM (harness resident-but-undriven), the gRPC analog of
    # the old `harness {kind:none}`. NOTE: the CLI create surface carries
    # no secrets/harness_env flags, so the old bogus ANTHROPIC_API_KEY dev
    # secret is no longer injected here — agent-mode dev sessions that need
    # a real key should bake it into the image's [secrets] instead.
    CREATE_ARGS=(session create --image "$IMAGE_URI")
    if [ "$HARNESS" = "none" ]; then
        CREATE_ARGS+=(--dev-vm)
        echo "==> session create (dev-vm: baked harness left undriven)"
    else
        echo "==> session create (driving the image's baked harness)"
    fi
    if [ -n "$PROMPT" ]; then
        CREATE_ARGS+=(--prompt "$PROMPT")
    fi
    SID=$(engrams "${CREATE_ARGS[@]}")
    echo "    session_id=$SID"
fi

echo ""
echo "✓ session $SID is live — iterate:"
echo ""
echo "  # exec a one-shot command"
echo "  engrams session exec $SID 'ls /'"
echo ""
echo "  # open the shell (browser, after port-forwarding)"
echo "  open http://localhost:5173/sessions/$SID"
echo ""
echo "  # raw events"
echo "  engrams session logs $SID"
echo ""
echo "  # tear down"
echo "  engrams session delete $SID"
