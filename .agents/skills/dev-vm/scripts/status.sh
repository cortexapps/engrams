#!/usr/bin/env bash
# One-screen view of the VM + sync state.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/config.sh"

echo "=== VM ==="
gcloud compute instances describe "$GCP_INSTANCE" "${gcloud_args[@]}" \
  --format='table[box](name, status, machineType.basename(), networkInterfaces[0].accessConfigs[0].natIP:label=EXTERNAL_IP)'

echo
echo "=== Mutagen ==="
if command -v mutagen >/dev/null 2>&1; then
  if mutagen sync list "$MUTAGEN_SESSION" 2>/dev/null; then :; else
    echo "no session named '$MUTAGEN_SESSION' — run sync-start.sh"
  fi
else
  echo "mutagen not installed locally — run bootstrap-local.sh"
fi
