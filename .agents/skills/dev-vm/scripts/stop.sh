#!/usr/bin/env bash
# Stop the dev VM. Disk persists (~$3/mo while stopped).
# Pauses any active Mutagen session first so the agent doesn't keep
# trying to reach a dead host.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/config.sh"

if command -v mutagen >/dev/null 2>&1 && mutagen sync list "$MUTAGEN_SESSION" >/dev/null 2>&1; then
  echo "pausing mutagen session $MUTAGEN_SESSION..."
  mutagen sync pause "$MUTAGEN_SESSION" || true
fi

echo "stopping $GCP_INSTANCE..."
gcloud compute instances stop "$GCP_INSTANCE" "${gcloud_args[@]}"
echo "stopped"
