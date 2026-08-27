#!/usr/bin/env bash
# Idempotently provision the cold-tier blob bucket inside the local
# emulators. ADR 0005 / Stage 4 (fake-gcs-server) + ADR 0122 (MinIO).
# Re-runs on every `tilt up`; already-exists is success everywhere.
#
# fake-gcs-server speaks the GCS JSON API but auto-creates buckets only
# in `-data` mode; in volume mode (which we use to persist across
# restarts) it requires explicit POST /storage/v1/b. The body shape
# mirrors the production GCS Buckets: insert API, just with the
# emulator's `localhost:4443` endpoint.
#
# The MinIO section runs only when an S3 test/dev endpoint is
# configured (`ENGRAM_TEST_S3_ENDPOINT` or `ENGRAM_S3_ENDPOINT_URL`),
# so GCS-only environments are unaffected. It signs requests via the
# `minio/mc` container (host network) — CreateBucket needs SigV4, so a
# bare curl can't do it the way the GCS section can.

set -euo pipefail

EMULATOR="${STORAGE_EMULATOR_HOST:-http://localhost:4443}"
S3_ENDPOINT="${ENGRAM_TEST_S3_ENDPOINT:-${ENGRAM_S3_ENDPOINT_URL:-}}"
# Honor either env var so the script works in both contexts:
#   - Local dev (Tiltfile): sets `ENGRAM_GCS_BUCKET` (the coordinator's
#     production env var name).
#   - CI / live tests: set `ENGRAM_TEST_GCS_BUCKET` (the round-trip
#     test's gating var; deliberately distinct from the prod name so
#     a misconfigured prod env var doesn't quietly hit a test bucket).
BUCKET="${ENGRAM_GCS_BUCKET:-${ENGRAM_TEST_GCS_BUCKET:-engram-snapshots-test}}"
S3_BUCKET="${ENGRAM_S3_BUCKET:-${ENGRAM_TEST_S3_BUCKET:-engram-snapshots-test}}"

seed_gcs() {
    # Wait briefly for the emulator to come up. Tilt's resource_deps
    # already gates this script behind the fake-gcs-server container, but
    # Docker's "container running" status flips before the HTTP server is
    # bound; one tight retry loop covers that gap.
    for i in 1 2 3 4 5; do
        if curl -sf "${EMULATOR}/storage/v1/b" -o /dev/null; then
            break
        fi
        if [ "$i" = "5" ]; then
            # An S3-only environment (MinIO configured, no GCS emulator
            # env) legitimately has no fake-gcs to seed — skip rather
            # than abort. Explicit STORAGE_EMULATOR_HOST still aborts:
            # a configured-but-dead emulator is a real failure.
            if [ -z "${STORAGE_EMULATOR_HOST:-}" ] && [ -n "${S3_ENDPOINT}" ]; then
                echo "seed-buckets: no fake-gcs-server at ${EMULATOR}; S3-only environment, skipping GCS seed"
                return 0
            fi
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
}

seed_s3() {
    # `mc mb --ignore-existing` is idempotent. Credentials default to
    # the MinIO root user the dev/CI containers run with.
    local access="${AWS_ACCESS_KEY_ID:-minioadmin}"
    local secret="${AWS_SECRET_ACCESS_KEY:-minioadmin}"
    for i in 1 2 3 4 5; do
        if curl -sf "${S3_ENDPOINT}/minio/health/live" -o /dev/null; then
            break
        fi
        if [ "$i" = "5" ]; then
            echo "minio not reachable at ${S3_ENDPOINT}; aborting" >&2
            exit 1
        fi
        sleep 1
    done
    docker run --rm --network host --entrypoint sh minio/mc -c \
        "mc alias set local '${S3_ENDPOINT}' '${access}' '${secret}' >/dev/null && \
         mc mb --ignore-existing local/'${S3_BUCKET}'"
    echo "seed-buckets: bucket '${S3_BUCKET}' ready on ${S3_ENDPOINT}"
}

seed_gcs
if [ -n "${S3_ENDPOINT}" ]; then
    seed_s3
fi
