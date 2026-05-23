#!/usr/bin/env bash
# Bake the canonical demo image to the local registry, register it
# with the running coord, and wait for the host-agent's prefetch
# supervisor to report the manifest_digest ready. Factored out of
# `integration-test.sh` so the CI e2e lane and the local smoke test
# share one bake choreography.
#
# Preconditions:
#   - integration-up.sh has been run (PG, registry, fake-gcs, coord,
#     host-agent are all up).
#   - host-target engram-cli at ./target/release/engram-cli.
#   - musl-target engram-agentd at
#     ./target/x86_64-unknown-linux-musl/release/engram-agentd.
#   (the script will cargo-build them if missing.)
#
# Side effects:
#   - Pushes a chunked-OCI artifact to localhost:5001 under
#     integration-test/demo:warm-<short-sha>.
#   - POSTs /api/enabled-images on the local coord at $COORD.
#
# On stdout, prints (and only prints) the final IMAGE_URI of the
# enabled image on success, so callers can:
#   IMAGE_URI=$(bash deploy/dev/integration-bake-demo.sh)
# Diagnostic chatter goes to stderr; the only stdout line is the URI.
#
# Env vars honoured:
#   COORD            — coord HTTP base URL; default http://127.0.0.1:8090
#   ENGRAM_TOKEN     — if set, bearer token sent on coord requests
#   READY_DEADLINE_SECS — max seconds to wait for digest-ready; default 240

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

COORD="${COORD:-http://127.0.0.1:8090}"
READY_DEADLINE_SECS="${READY_DEADLINE_SECS:-240}"

AUTH_HEADER=()
if [ -n "${ENGRAM_TOKEN:-}" ]; then
    AUTH_HEADER=(-H "Authorization: Bearer $ENGRAM_TOKEN")
fi

log() { echo "$@" >&2; }

if ! curl -fsS "$COORD/healthz" >/dev/null 2>&1; then
    log "ERROR: coord not reachable at $COORD"
    log "       Run 'just integration-up' first."
    exit 1
fi

SHORT=$(git rev-parse --short HEAD)
LOCAL_REGISTRY="localhost:5001"
IMAGE_URI="$LOCAL_REGISTRY/integration-test/demo:warm-$SHORT"

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
    --repo integration-test/demo \
    --tag "warm-$SHORT" \
    --source deploy/demo \
    --format ext4 \
    --images-dir ./var/integration/images \
    --inject-agent target/x86_64-unknown-linux-musl/release/engram-agentd \
    --push "$IMAGE_URI" \
    >&2 2>&1
T1=$(date +%s.%N)
log "    bake+push elapsed: $(echo "$T1 - $T0" | bc)s"

log ""
log "==> step 2/3: POST /api/enabled-images"
ENABLE_BODY=$(printf '{"image_uri": "%s"}' "$IMAGE_URI")
T0=$(date +%s.%N)
curl -fsS -X POST "${AUTH_HEADER[@]}" \
    -H "Content-Type: application/json" \
    -d "$ENABLE_BODY" \
    "$COORD/api/enabled-images" \
    >/dev/null
T1=$(date +%s.%N)
log "    enable elapsed: $(echo "$T1 - $T0" | bc)s"

log ""
log "==> step 3/3: poll /api/hosts until this image's digest is ready"
# ADR 0015 M5: the host-agent's prefetch supervisor pulls chunks
# from BlobStorage in the background. We need to wait for THIS
# image's manifest_digest (each bake produces a unique one) to
# appear in some host's ready_image_digests, not just for the
# count to be >= 1 — a prior run's image may already be ready.
EXPECTED_DIGEST=$(curl -fsS "${AUTH_HEADER[@]}" "$COORD/api/enabled-images" \
    | python3 -c "import sys,json; rows = json.load(sys.stdin).get('images', []); print(next((r['manifest_digest'] for r in rows if r['image_uri']=='$IMAGE_URI'), ''))")
if [ -z "$EXPECTED_DIGEST" ]; then
    log "ERROR: couldn't read manifest_digest from /api/enabled-images"
    exit 1
fi
log "    waiting for digest: $EXPECTED_DIGEST"
DEADLINE=$(( $(date +%s) + READY_DEADLINE_SECS ))
T0=$(date +%s.%N)
while :; do
    READY=$(curl -fsS "${AUTH_HEADER[@]}" "$COORD/api/hosts" \
        | python3 -c "import sys,json; rows = json.load(sys.stdin).get('hosts', []); digests = {d for r in rows for d in r.get('ready_image_digests', [])}; print('yes' if '$EXPECTED_DIGEST' in digests else 'no')")
    if [ "$READY" = "yes" ]; then
        T1=$(date +%s.%N)
        log "    host reported digest ready after $(echo "$T1 - $T0" | bc)s ✓"
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
