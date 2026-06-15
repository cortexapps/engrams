#!/usr/bin/env bash
# Bake the canonical demo image to the local registry, register it
# with the running coord via the app gRPC surface (ADR 0051), and wait
# for the host-agent's prefetch supervisor to report the manifest_digest
# ready. Factored out of `integration-test.sh` so the CI e2e lane and
# the local smoke test share one bake choreography.
#
# Preconditions:
#   - the stack is up via `just dev` (PG, registry, fake-gcs, coord,
#     host-agent are all up).
#   - host-target engram-cli at ./target/release/engram-cli.
#   - musl-target engram-agentd at
#     ./target/x86_64-unknown-linux-musl/release/engram-agentd.
#   (the script will cargo-build them if missing.)
#
# Side effects:
#   - Pushes a chunked-OCI artifact to localhost:5001 under
#     integration-test/demo-claude:warm-<short-sha>.
#   - Calls `engram-cli image enable` on the local coord at the app
#     gRPC address (ENGRAM_APP_GRPC, default http://127.0.0.1:50061).
#
# On stdout, prints (and only prints) the final IMAGE_URI of the
# enabled image on success, so callers can:
#   IMAGE_URI=$(bash deploy/dev/integration-bake-demo.sh)
# Diagnostic chatter goes to stderr; the only stdout line is the URI.
#
# Env vars honoured:
#   COORD                — coord HTTP base URL (healthz only); default http://127.0.0.1:8090
#   ENGRAM_APP_GRPC      — app gRPC address; default http://127.0.0.1:50061
#   ENGRAM_APP_TOKEN     — if set, bearer token sent on app gRPC requests
#   READY_DEADLINE_SECS  — max seconds to wait for digest-ready; default 240

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

COORD="${COORD:-http://127.0.0.1:8090}"
READY_DEADLINE_SECS="${READY_DEADLINE_SECS:-240}"

# gRPC address and bearer token for the app surface (ADR 0051).
export ENGRAM_APP_GRPC="${ENGRAM_APP_GRPC:-http://127.0.0.1:50061}"

# Fail-closed app surface (ADR 0051 Task 9): token always required;
# default to the Tiltfile dev literal, override via env elsewhere.
ENGRAM_APP_TOKEN="${ENGRAM_APP_TOKEN:-dev-app-grpc-token}"
CLI_TOKEN_FLAG=(--token "$ENGRAM_APP_TOKEN")

log() { echo "$@" >&2; }

if ! curl -fsS "$COORD/healthz" >/dev/null 2>&1; then
    log "ERROR: coord not reachable at $COORD"
    log "       Run 'just dev' first."
    exit 1
fi

SHORT=$(git rev-parse --short HEAD)
LOCAL_REGISTRY="localhost:5001"
# ADR 0021: bake the harnessed variant (`demo-claude`) so the
# integration stack exercises the baked-harness flow end-to-end.
IMAGE_URI="$LOCAL_REGISTRY/integration-test/demo-claude:warm-$SHORT"

log "==> step 1/3: bake demo image"
log "    target: $IMAGE_URI"

# Re-build only if the binaries are missing; the CI lane downloads
# release artifacts produced by an upstream job, and we want to
# respect those rather than re-compile.
if [ ! -x ./target/release/engram-cli ]; then
    log "    building engram-cli (host) — not present in target/release"
    cargo build --release -p engram-cli >&2
fi
if [ ! -x ./target/x86_64-unknown-linux-musl/release/engram-agentd ]; then
    log "    building engram-agentd (musl) — not present in target/x86_64-unknown-linux-musl/release"
    cargo build --release --target x86_64-unknown-linux-musl -p engram-agentd >&2
fi

T0=$(date +%s.%N)
./target/release/engram-cli image build \
    --repo integration-test/demo-claude \
    --tag "warm-$SHORT" \
    --source deploy/demo-claude \
    --format ext4 \
    --images-dir ./var/integration/images \
    --inject-agent target/x86_64-unknown-linux-musl/release/engram-agentd \
    --push "$IMAGE_URI" \
    >&2 2>&1
T1=$(date +%s.%N)
log "    bake+push elapsed: $(echo "$T1 - $T0" | bc)s"

log ""
log "==> step 2/3: enable image (ADR 0036: async — polls to completion)"
T0=$(date +%s.%N)
./target/release/engram-cli ${CLI_TOKEN_FLAG[@]+"${CLI_TOKEN_FLAG[@]}"} \
    image enable --uri "$IMAGE_URI" >&2
T1=$(date +%s.%N)
log "    enable elapsed: $(echo "$T1 - $T0" | bc)s"

log ""
log "==> step 3/3: poll hosts until this image's digest is ready"
# ADR 0015 M5: the host-agent's prefetch supervisor pulls chunks
# from BlobStorage in the background. We need to wait for THIS
# image's manifest_digest (each bake produces a unique one) to
# appear in some host's ready_image_digests, not just for the
# count to be >= 1 — a prior run's image may already be ready.
EXPECTED_DIGEST=$(./target/release/engram-cli ${CLI_TOKEN_FLAG[@]+"${CLI_TOKEN_FLAG[@]}"} --json image list \
    | python3 -c "
import sys,json
rows = json.load(sys.stdin).get('images', [])
print(next((r['manifest_digest'] for r in rows if r['image_uri']=='$IMAGE_URI'), ''))
")
if [ -z "$EXPECTED_DIGEST" ]; then
    log "ERROR: couldn't read manifest_digest from image list"
    exit 1
fi
log "    waiting for digest: $EXPECTED_DIGEST"
DEADLINE=$(( $(date +%s) + READY_DEADLINE_SECS ))
T0=$(date +%s.%N)
while :; do
    READY=$(./target/release/engram-cli ${CLI_TOKEN_FLAG[@]+"${CLI_TOKEN_FLAG[@]}"} --json host list \
        | python3 -c "
import sys,json
rows = json.load(sys.stdin).get('hosts', [])
digests = {d for r in rows for d in r.get('ready_image_digests', [])}
print('yes' if '$EXPECTED_DIGEST' in digests else 'no')
")
    if [ "$READY" = "yes" ]; then
        T1=$(date +%s.%N)
        log "    host reported digest ready after $(echo "$T1 - $T0" | bc)s"
        break
    fi
    if [ "$(date +%s)" -ge "$DEADLINE" ]; then
        log "ERROR: no host reported digest $EXPECTED_DIGEST ready within ${READY_DEADLINE_SECS}s"
        log "       host-agent prefetch may have stalled; check logs"
        exit 1
    fi
    sleep 1
done

# Only the IMAGE_URI goes to stdout — callers parse this single line.
echo "$IMAGE_URI"
