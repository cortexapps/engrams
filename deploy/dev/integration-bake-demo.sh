#!/usr/bin/env bash
# Bake the canonical demo image to the local registry, register it
# with the running coord, and wait for the host-agent's prefetch
# supervisor to report the manifest_digest ready. Factored out of
# `integration-test.sh` so the CI e2e lane and the local smoke test
# share one bake choreography.
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
#     integration-test/demo:warm-<short-sha>.
#   - Enables the image on the local coord via app-gRPC ImageService
#     (ADR 0051 — the HTTP /api/v1/enabled-images route is gone).
#
# On stdout, prints (and only prints) the final IMAGE_URI of the
# enabled image on success, so callers can:
#   IMAGE_URI=$(bash deploy/dev/integration-bake-demo.sh)
# Diagnostic chatter goes to stderr; the only stdout line is the URI.
#
# Env vars honoured:
#   COORD            — coord HTTP base URL (healthz reachability only);
#                      default http://127.0.0.1:8090
#   ENGRAM_APP_GRPC  — coord app-gRPC addr for the CLI; default
#                      http://127.0.0.1:50061
#   ENGRAM_APP_TOKEN — bearer for the app-gRPC surface (must match the
#                      coord's ENGRAM_APP_GRPC_TOKENS); also accepts
#                      ENGRAM_E2E_GRPC_TOKEN as a fallback.
#   READY_DEADLINE_SECS — max seconds to wait for digest-ready; default 240

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

COORD="${COORD:-http://127.0.0.1:8090}"
READY_DEADLINE_SECS="${READY_DEADLINE_SECS:-240}"

# ADR 0051: image enable + the enabled-images / hosts reads moved to
# app-gRPC. The CLI reads ENGRAM_APP_GRPC + ENGRAM_APP_TOKEN; export both
# so the `engram-cli image enable/list` and `host list` calls below
# authenticate against the coord's fail-closed bearer.
export ENGRAM_APP_GRPC="${ENGRAM_APP_GRPC:-http://127.0.0.1:50061}"
export ENGRAM_APP_TOKEN="${ENGRAM_APP_TOKEN:-${ENGRAM_E2E_GRPC_TOKEN:-dev-app-grpc-token}}"

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
# The image has `[harness] builtin = "claude"` in its engram.toml;
# the baker pulls the published harness-claude artifact from GHCR
# and injects it into /opt/engram/harness/ during the bake. No
# separate harness-registration step is needed downstream — the
# image carries its harness contract in manifest.toml.
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
log "==> step 2/3: enable image via app-gRPC ImageService (ADR 0036 async; the CLI polls the job to terminal)"
# `engram-cli image enable` (without --no-wait) submits the EnableImage
# RPC and internally polls GetEnableJob until the job reaches a terminal
# state, returning non-zero on `failed`. ADR 0051: this replaces the old
# POST /api/v1/enabled-images + manual job-poll loop.
T0=$(date +%s.%N)
if ! ./target/release/engram-cli image enable --uri "$IMAGE_URI" >&2; then
    log "ERROR: image enable failed for $IMAGE_URI (see CLI output above)"
    exit 1
fi
T1=$(date +%s.%N)
log "    enable elapsed: $(echo "$T1 - $T0" | bc)s"

log ""
log "==> step 3/3: poll ListHosts until this image's digest is ready"
# ADR 0015 M5: the host-agent's prefetch supervisor pulls chunks
# from BlobStorage in the background. We need to wait for THIS
# image's manifest_digest (each bake produces a unique one) to
# appear in some host's ready_image_digests, not just for the
# count to be >= 1 — a prior run's image may already be ready.
# ADR 0051: read the digest + host readiness over app-gRPC
# (ImageService::ListEnabledImages / FleetService::ListHosts) via the
# CLI `--json` views, which mirror the old HTTP JSON shapes.
EXPECTED_DIGEST=$(./target/release/engram-cli image list --json \
    | python3 -c "import sys,json; rows = json.load(sys.stdin).get('images', []); print(next((r['manifest_digest'] for r in rows if r['image_uri']=='$IMAGE_URI'), ''))")
if [ -z "$EXPECTED_DIGEST" ]; then
    log "ERROR: couldn't read manifest_digest from ImageService::ListEnabledImages"
    exit 1
fi
log "    waiting for digest: $EXPECTED_DIGEST"
DEADLINE=$(( $(date +%s) + READY_DEADLINE_SECS ))
T0=$(date +%s.%N)
while :; do
    READY=$(./target/release/engram-cli host list --json \
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
