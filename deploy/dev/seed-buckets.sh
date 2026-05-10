#!/usr/bin/env bash
# Idempotently provision the cold-tier blob bucket inside
# fake-gcs-server. ADR 0005 / Stage 4. Re-runs on every `tilt up`;
# 200 (created) and 409 (already exists) are both success.
#
# fake-gcs-server speaks the GCS JSON API but auto-creates buckets only
# in `-data` mode; in volume mode (which we use to persist across
# restarts) it requires explicit POST /storage/v1/b. The body shape
# mirrors the production GCS Buckets: insert API, just with the
# emulator's `localhost:4443` endpoint.

set -euo pipefail

EMULATOR="${STORAGE_EMULATOR_HOST:-http://localhost:4443}"
# Honor either env var so the script works in both contexts:
#   - Local dev (Tiltfile): sets `ENGRAM_GCS_BUCKET` (the coordinator's
#     production env var name).
#   - CI / live tests: set `ENGRAM_TEST_GCS_BUCKET` (the round-trip
#     test's gating var; deliberately distinct from the prod name so
#     a misconfigured prod env var doesn't quietly hit a test bucket).
BUCKET="${ENGRAM_GCS_BUCKET:-${ENGRAM_TEST_GCS_BUCKET:-engram-snapshots-test}}"

# Wait briefly for the emulator to come up. Tilt's resource_deps
# already gates this script behind the fake-gcs-server container, but
# Docker's "container running" status flips before the HTTP server is
# bound; one tight retry loop covers that gap.
for i in 1 2 3 4 5; do
    if curl -sf "${EMULATOR}/storage/v1/b" -o /dev/null; then
        break
    fi
    if [ "$i" = "5" ]; then
        echo "fake-gcs-server not reachable at ${EMULATOR}; aborting" >&2
        exit 1
    fi
    sleep 1
done

# POST the bucket. The `name` field is what the SDK references; the
# `location` and `storageClass` defaults match what the prod-shape
# tooling expects but are otherwise irrelevant under the emulator.
HTTP_STATUS=$(curl -sw '%{http_code}' -o /tmp/seed-buckets.out \
    -X POST "${EMULATOR}/storage/v1/b?project=engram-dev" \
    -H 'Content-Type: application/json' \
    -d "{\"name\":\"${BUCKET}\"}" || true)

case "${HTTP_STATUS}" in
    200|201)
        echo "seed-buckets: created bucket '${BUCKET}' on ${EMULATOR}"
        ;;
    409)
        echo "seed-buckets: bucket '${BUCKET}' already exists on ${EMULATOR}"
        ;;
    *)
        echo "seed-buckets: unexpected status ${HTTP_STATUS} from emulator:" >&2
        cat /tmp/seed-buckets.out >&2
        exit 1
        ;;
esac
