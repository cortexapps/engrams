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
# Cleanup: `just dev-down` reaps the session along with everything
# else; or `engram-cli session delete $SID` directly.

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# Dev bakes cross-compile the rootfs for the host's own arch (mirrors
# bake-demo.sh).
case "$(uname -m)" in
    arm64 | aarch64)
        TARGET=aarch64-unknown-linux-musl
        ;;
    x86_64 | amd64)
        TARGET=x86_64-unknown-linux-musl
        ;;
    *)
        echo "integration-session: unsupported arch $(uname -m)" >&2
        exit 1
        ;;
esac

COORD="http://127.0.0.1:8090"
HARNESS="${HARNESS:-none}"
PROMPT="${PROMPT:-}"

# ADR 0051 Drip E: the coordinator's web-facing REST surface is gone —
# enable/list images, list sessions, create + delete sessions all go
# over the coord's app-gRPC via engram-cli. Endpoint + bearer default
# to the Tiltfile's coord app-gRPC. (The `/healthz` check below is a
# KEPT REST route, left as-is.)
ENGRAM_CLI="${ENGRAM_INTEG_BIN_DIR:-./target/release}/engram-cli"
export ENGRAM_APP_GRPC_ADDR="${ENGRAM_APP_GRPC_ADDR:-http://127.0.0.1:50061}"
export ENGRAM_APP_GRPC_TOKENS="${ENGRAM_APP_GRPC_TOKENS:-dev-app-grpc-token}"

if ! curl -fsS "$COORD/healthz" >/dev/null 2>&1; then
    echo "ERROR: coord not reachable at $COORD" >&2
    echo "       Run 'just dev' first." >&2
    exit 1
fi

SHORT=$(git rev-parse --short HEAD)
LOCAL_REGISTRY="localhost:5001"
IMAGE_URI="$LOCAL_REGISTRY/integration-test/demo:warm-$SHORT"

# Already enabled?
already_enabled=$("$ENGRAM_CLI" --json image list 2>/dev/null \
    | python3 -c "import sys,json; d=json.load(sys.stdin); print(any(i.get('image_uri')=='$IMAGE_URI' for i in d.get('images',[])))" 2>/dev/null || echo False)

if [ "$already_enabled" = "True" ]; then
    echo "==> image $IMAGE_URI already enabled; skipping bake"
else
    echo "==> baking demo image @ $SHORT"
    cargo build --release -p engram-cli >/dev/null 2>&1
    cargo build --release --target "$TARGET" \
        -p engram-agentd >/dev/null 2>&1
    "$ENGRAM_CLI" image build \
        --repo integration-test/demo \
        --tag "warm-$SHORT" \
        --source deploy/demo \
        --format ext4 \
        --images-dir ./var/integration/images \
        --inject-agent "target/$TARGET/release/engram-agentd" \
        --push "$IMAGE_URI" \
        2>&1 | tail -3
    # Base snapshot is captured at enable time (ADR 0020), not at bake — the
    # old --capture-canonical-* flags were removed from `engram-cli image build`.

    echo "==> enable image over app-gRPC (blocks until the enable job is ready)"
    # ADR 0051 + ADR 0036: `image enable` enables AND polls the async
    # enable job internally, returning non-zero on job failure.
    if ! "$ENGRAM_CLI" image enable --uri "$IMAGE_URI"; then
        echo "ERROR: enabling $IMAGE_URI failed (enable job did not reach ready)" >&2
        exit 1
    fi
    echo "    enable job ready"
fi

# Look for an existing dev-session row (status=active, this image,
# tagged via metadata-style harness so we can find it). The session
# row doesn't carry a label field, so we filter by image+status and
# pick the most recent. Best-effort: stale rows from prior runs are
# possible if `just dev-down` didn't reap.
existing_sid=$("$ENGRAM_CLI" --json session list 2>/dev/null \
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
    SID=$("$ENGRAM_CLI" "${CREATE_ARGS[@]}")
    echo "    session_id=$SID"
fi

echo ""
echo "✓ session $SID is live — iterate:"
echo ""
echo "  # exec a one-shot command"
echo "  $ENGRAM_CLI session exec $SID 'ls /'"
echo ""
echo "  # open the shell (browser, after port-forwarding)"
echo "  open http://localhost:5173/sessions/$SID"
echo ""
echo "  # raw events"
echo "  $ENGRAM_CLI session logs $SID"
echo ""
echo "  # tear down"
echo "  $ENGRAM_CLI session delete $SID"
