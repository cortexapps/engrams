#!/usr/bin/env bash
# CI teardown counterpart to tilt-up-ci.sh: stop the Tilt-managed
# processes + docker-compose services, then restore ownership of files
# the sudo'd host-agent may have written as root (so the cargo-cache
# action can read target/ afterwards). Best-effort; never fails the job.

set -uo pipefail
cd "$(git rev-parse --show-toplevel)" || exit 0

tilt down 2>&1 | tail -20 || true
sudo chown -R "$USER":"$USER" target/ var/ ~/.cargo ~/.docker 2>/dev/null || true
exit 0
