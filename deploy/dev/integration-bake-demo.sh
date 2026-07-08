#!/usr/bin/env bash
# Build + push the canonical demo image to the local registry as a
# PLAIN docker image (ADR 0080 phase 3b — the engram-artifact bake is
# retired from this path; enable materializes host-side), register it
# with the running coord, and wait for the host-agent's prefetch
# supervisor to report the manifest_digest ready. Factored out of
# `integration-test.sh` so the CI e2e lane and the local smoke test
# share one bake choreography.
#
# Preconditions:
#   - the stack is up via `just dev` (PG, registry, fake-gcs, coord,
#     host-agent are all up).
#   - docker (for `docker build && docker push`).
#   - host-target engram-cli at ./target/release/engram-cli (for
#     `image enable` + the readiness polls).
#   (the script will cargo-build engram-cli if missing.)
#
# Side effects:
#   - Pushes a standard docker image to localhost:5001 under
#     integration-test/demo:warm-<short-sha>.
#   - Enables the image on the local coord over app-gRPC
#     (`engram-cli image enable`) — the coordinator drives the
#     host-side MaterializeImage + base-snapshot capture.
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

# ADR 0051 Drip E: the coordinator's web-facing REST surface (POST
# /api/v1/enabled-images, GET /api/v1/enable-jobs/:id, GET
# /api/v1/enabled-images, GET /api/v1/hosts) is gone — enable + read
# enabled images + read host digest-readiness over the coord's
# app-gRPC via engram-cli. Endpoint + bearer default to the Tiltfile's
# coord app-gRPC; the workflow may override via
# ENGRAM_APP_GRPC_ADDR/TOKENS. (The `/healthz` check below is a KEPT
# REST route, left as-is.)
ENGRAM_CLI="${ENGRAM_INTEG_BIN_DIR:-./target/release}/engram-cli"
export ENGRAM_APP_GRPC_ADDR="${ENGRAM_APP_GRPC_ADDR:-http://127.0.0.1:50061}"
export ENGRAM_APP_GRPC_TOKENS="${ENGRAM_APP_GRPC_TOKENS:-dev-app-grpc-token}"

AUTH_HEADER=()
if [ -n "${ENGRAM_TOKEN:-}" ]; then
    AUTH_HEADER=(-H "Authorization: Bearer $ENGRAM_TOKEN")
fi

log() { echo "$@" >&2; }

if ! curl -fsS "$COORD/healthz" >/dev/null 2>&1; then
    log "ERROR: coord not reachable at $COORD"
    log "       Run 'just dev' first."
    exit 1
fi

SHORT=$(git rev-parse --short HEAD)
LOCAL_REGISTRY="localhost:5001"
# Build the canonical `demo` image. ADR 0062: the image carries NO harness —
# the built-in `claude` harness is a per-session selection that rides the fleet
# `current_bundles` stamp (the e2e stages it via ENGRAM_HARNESS_CLAUDE_TREE +
# `just bundles-squashfs`), not baked in here. ADR 0080: no agentd, no init
# shim, no engram tooling in the image at all — enable-time materialization
# injects the stage-1 shim and agentd rides its bundle slot.
IMAGE_URI="$LOCAL_REGISTRY/integration-test/demo:warm-$SHORT"

log "==> step 1/3: docker build + push demo image"
log "    target: $IMAGE_URI"

# Re-build only if the binary is missing; the CI lane downloads
# release artifacts produced by an upstream job, and we want to
# respect those rather than re-compile.
if [ ! -x ./target/release/engram-cli ]; then
    log "    building engram-cli (host) — not present in target/release"
    cargo build --release -p engram-cli >&2
fi

# ADR 0080 phase 3b: a PLAIN `docker build && docker push` — the exact
# user contract the materializer consumes. Keep bake-demo.sh's arm64
# FROM pin: on Apple Silicon the guest is arm64, and docker would
# otherwise build the host's default (amd64 under emulation on some
# setups), producing an image the arm64 fleet can't materialize.
case "$(uname -m)" in
    arm64 | aarch64) ARM=1 ;;
    x86_64 | amd64) ARM=0 ;;
    *)
        log "ERROR: unsupported host arch $(uname -m)"
        exit 1
        ;;
esac
STAGING="./var/integration/demo-build"
rm -rf "$STAGING"
mkdir -p "$STAGING"
cp -R deploy/demo/. "$STAGING/"
if [ "$ARM" = "1" ]; then
    sed -i.bak 's|^FROM |FROM --platform=linux/arm64 |' "$STAGING/Dockerfile"
    rm -f "$STAGING/Dockerfile.bak"
fi

T0=$(date +%s.%N)
docker build -t "$IMAGE_URI" "$STAGING" >&2
docker push "$IMAGE_URI" >&2
T1=$(date +%s.%N)
log "    build+push elapsed: $(echo "$T1 - $T0" | bc)s"

log ""
log "==> step 2/3: enable image over app-gRPC (blocks until the enable job is ready)"
# ADR 0051 + ADR 0036: `image enable` enables AND polls the async
# enable job internally, returning non-zero on job failure. This
# collapses the old "POST /enabled-images then poll /enable-jobs/:id"
# into one command. `set -e` aborts on a non-zero exit.
T0=$(date +%s.%N)
if ! "$ENGRAM_CLI" image enable --uri "$IMAGE_URI" --config deploy/demo/image-config.toml >&2; then
    log "ERROR: enabling $IMAGE_URI failed (enable job did not reach ready)"
    exit 1
fi
T1=$(date +%s.%N)
log "    enable elapsed: $(echo "$T1 - $T0" | bc)s"

log ""
log "==> step 3/3: poll hosts until this image's digest is ready"
# ADR 0015 M5: the host-agent's prefetch supervisor pulls chunks
# from BlobStorage in the background. We need to wait for THIS
# image's manifest_digest (each bake produces a unique one) to
# appear in some host's ready_image_digests, not just for the
# count to be >= 1 — a prior run's image may already be ready.
EXPECTED_DIGEST=$("$ENGRAM_CLI" --json image list \
    | python3 -c "import sys,json; rows = json.load(sys.stdin).get('images', []); print(next((r['manifest_digest'] for r in rows if r['image_uri']=='$IMAGE_URI'), ''))")
if [ -z "$EXPECTED_DIGEST" ]; then
    log "ERROR: couldn't read manifest_digest from 'engram-cli image list'"
    exit 1
fi
log "    waiting for digest: $EXPECTED_DIGEST"
DEADLINE=$(( $(date +%s) + READY_DEADLINE_SECS ))
T0=$(date +%s.%N)
while :; do
    READY=$("$ENGRAM_CLI" --json hosts list \
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
