#!/usr/bin/env bash
# Hard-reset the integration stack to a clean slate.
#
# Use when prior runs have left state that's interfering with the
# current run — most commonly stale `templates` rows pointing at
# blob keys that aren't in the fake-gcs bucket anymore (whose
# warm-pool refills then spam the host-agent log and can starve
# the test's real session create).
#
# What this nukes:
#   • coord + host-agent processes (via integration-down)
#   • docker compose volumes (postgres data, fake-gcs bucket
#     contents, registry blobs) — `down -v`
#   • ./var/integration                 — log/pid dir
#   • ./var/host-sandboxes-integration  — per-sandbox state, FC
#                                         jails, chunked-rootfs cache
#   • ./var/engram-integration          — coord-side local blob/cache
#   • ./var/sandboxes                   — legacy mode=all sandbox dir
#   • ./var/engram                      — legacy mode=all local store
#   • ./var/bake                        — bake staging
#
# What this preserves:
#   • .env (KEK lives here; surviving lets re-bring-up skip the
#     openssl roll)
#   • ~/.cache/engram-fc-test/         — FC kernel + ubuntu rootfs
#   • target/                          — Rust build artifacts
#   • the OCI registry image data isn't actually nuked unless we
#     -v down, which we do — but if you want to keep images,
#     re-push them (or set ENGRAM_KEEP_REGISTRY=1 below).
#
# After this completes, run `just integration-up` for a fresh
# stack.

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# Stop everything first. integration-down kills the processes and
# composes-down (without -v).
bash deploy/dev/integration-down.sh

echo ""
echo "==> docker compose down -v (drops postgres + fake-gcs volumes)"
docker compose -f deploy/docker-compose.dev.yml -f deploy/docker-compose.linux.yml down -v

echo ""
echo "==> wiping ./var dirs"
# host-sandboxes-integration may contain FC jail roots created
# under sudo (different uid). Need sudo to rm.
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
    SUDO="sudo"
fi
$SUDO rm -rf \
    ./var/integration \
    ./var/host-sandboxes-integration \
    ./var/engram-integration \
    ./var/sandboxes \
    ./var/engram \
    ./var/bake \
    2>/dev/null || true

echo ""
echo "✓ integration stack reset to clean slate"
echo "  next: just integration-up"
