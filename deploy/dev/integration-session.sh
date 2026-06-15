#!/usr/bin/env bash
# Persistent dev session — bakes + enables + creates a session and
# leaves it running so you can poke at it with engram-cli, wscat, or
# the web UI. Companion to `integration-test.sh` (which always deletes
# at the end).
#
# Idempotent across iterations:
#   • If the demo image at the current HEAD is already enabled,
#     skip the bake/enable.
#   • If a session from a previous run is still active, print its
#     id and exit — don't keep stacking.
#
# Common invocations:
#   bash deploy/dev/integration-session.sh            # default (dev_vm)
#   HARNESS=claude bash deploy/dev/integration-session.sh
#   PROMPT='hi' HARNESS=claude bash deploy/dev/integration-session.sh
#
# Cleanup: `just dev-down` reaps the session along with everything
# else; or `engram-cli session delete $SID` directly.

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
INTEG_DIR="./var/integration"
COORD="http://127.0.0.1:8090"
HARNESS="${HARNESS:-none}"
PROMPT="${PROMPT:-}"

# gRPC address and bearer token for the app surface (ADR 0051).
# Set ENGRAM_APP_GRPC to override; default matches the Tiltfile.
export ENGRAM_APP_GRPC="${ENGRAM_APP_GRPC:-http://127.0.0.1:50061}"
# The app-gRPC surface is FAIL-CLOSED (ADR 0051 Task 9) — a token is
# always required. Default to the Tiltfile's dev literal so `just dev`
# workflows stay zero-config; CI/other envs override via env.
ENGRAM_APP_TOKEN="${ENGRAM_APP_TOKEN:-dev-app-grpc-token}"
CLI_TOKEN_FLAG=(--token "$ENGRAM_APP_TOKEN")

if ! curl -fsS "$COORD/healthz" >/dev/null 2>&1; then
    echo "ERROR: coord not reachable at $COORD" >&2
    echo "       Run 'just dev' first." >&2
    exit 1
fi

SHORT=$(git rev-parse --short HEAD)
LOCAL_REGISTRY="localhost:5001"
IMAGE_URI="$LOCAL_REGISTRY/integration-test/demo:warm-$SHORT"

# Already enabled? Use the CLI to check via gRPC.
already_enabled=$(./target/release/engram-cli ${CLI_TOKEN_FLAG[@]+"${CLI_TOKEN_FLAG[@]}"} --json image list 2>/dev/null \
    | python3 -c "
import sys,json
try:
    d=json.load(sys.stdin)
except Exception:
    d={}
print(any(i.get('image_uri')=='$IMAGE_URI' for i in d.get('images',[])))
" 2>/dev/null || echo False)

if [ "$already_enabled" = "True" ]; then
    echo "==> image $IMAGE_URI already enabled; skipping bake"
else
    # Backend + arch detection (ported from bake-demo.sh): dev bakes
    # cross-compile agentd for the host's own arch (guest arch == host
    # arch), and the in-VM transport depends on the backend (VZ uses
    # virtio-console; Firecracker/process use vsock). The old hardcoded
    # x86_64 musl target baked an image that can't boot on VZ/arm64.
    backend="$(bash deploy/dev/detect-backend.sh)"
    case "$(uname -m)" in
        arm64 | aarch64) TARGET=aarch64-unknown-linux-musl ;;
        x86_64 | amd64) TARGET=x86_64-unknown-linux-musl ;;
        *)
            echo "integration-session: unsupported arch $(uname -m)" >&2
            exit 1
            ;;
    esac
    if [ "$backend" = "vz" ]; then TRANSPORT=console; else TRANSPORT=vsock; fi

    # On macOS/VZ, ext4 image builds need mke2fs (e2fsprogs).
    if [ "$backend" = "vz" ]; then
        PATH="/opt/homebrew/opt/e2fsprogs/sbin:$PATH"
        if ! command -v mke2fs >/dev/null 2>&1; then
            echo "ERROR: mke2fs not found. VZ image builds require e2fsprogs:" >&2
            echo "       brew install e2fsprogs" >&2
            exit 1
        fi
    fi

    rustup target add "$TARGET" >/dev/null 2>&1 || true

    echo "==> baking demo image @ $SHORT ($TARGET, transport=$TRANSPORT)"
    cargo build --release -p engram-cli >/dev/null 2>&1
    cargo build --release --target "$TARGET" \
        -p engram-agentd >/dev/null 2>&1
    ./target/release/engram-cli image build \
        --repo integration-test/demo \
        --tag "warm-$SHORT" \
        --source deploy/demo \
        --format ext4 \
        --images-dir ./var/integration/images \
        --transport "$TRANSPORT" \
        --inject-agent "target/$TARGET/release/engram-agentd" \
        --push "$IMAGE_URI" \
        2>&1 | tail -3
    # Base snapshot is captured at enable time (ADR 0020), not at bake.

    echo "==> enabling image (ADR 0036: async — polls to completion)"
    ./target/release/engram-cli ${CLI_TOKEN_FLAG[@]+"${CLI_TOKEN_FLAG[@]}"} \
        image enable --uri "$IMAGE_URI"
fi

# Look for an existing dev-session row (status=active, this image).
# The session row carries the image URI, so we filter by image+status
# and pick the first match. Best-effort: stale rows from prior runs are
# possible if `just dev-down` didn't reap.
existing_sid=$(./target/release/engram-cli ${CLI_TOKEN_FLAG[@]+"${CLI_TOKEN_FLAG[@]}"} --json session list 2>/dev/null \
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
    echo "==> creating session (harness=$HARNESS)"
    # ADR 0021 P1.3: mode=dev_vm leaves the baked harness undriven.
    # HARNESS=none (the default) → --dev-vm; HARNESS=<name> → agent mode
    # (the image's baked harness drives automatically).
    if [ "$HARNESS" = "none" ]; then
        MODE_FLAG=(--dev-vm)
    else
        MODE_FLAG=()
    fi
    if [ -n "$PROMPT" ]; then
        PROMPT_FLAG=(--prompt "$PROMPT")
    else
        PROMPT_FLAG=()
    fi
    SID=$(./target/release/engram-cli ${CLI_TOKEN_FLAG[@]+"${CLI_TOKEN_FLAG[@]}"} \
        session create \
        --image "$IMAGE_URI" \
        ${MODE_FLAG[@]+"${MODE_FLAG[@]}"} \
        ${PROMPT_FLAG[@]+"${PROMPT_FLAG[@]}"})
    echo "    session_id=$SID"
fi

echo ""
echo "✓ session $SID is live — iterate:"
echo ""
echo "  # exec a one-shot command"
echo "  ./target/release/engram-cli session exec $SID 'ls /'"
echo ""
echo "  # open the shell (browser, after port-forwarding)"
echo "  open http://localhost:5173/sessions/$SID"
echo ""
echo "  # tail live events"
echo "  ./target/release/engram-cli session logs $SID"
echo ""
echo "  # tear down"
echo "  ./target/release/engram-cli session delete $SID"
