#!/usr/bin/env bash
# Start the dev VM and wait until SSH is accepting connections.
# Idempotent: a no-op if the instance is already RUNNING.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/config.sh"

status="$(gcloud compute instances describe "$GCP_INSTANCE" "${gcloud_args[@]}" --format='value(status)' 2>/dev/null || echo MISSING)"
case "$status" in
  RUNNING)    echo "$GCP_INSTANCE already RUNNING" ;;
  TERMINATED) echo "starting $GCP_INSTANCE..."; gcloud compute instances start "$GCP_INSTANCE" "${gcloud_args[@]}" ;;
  MISSING)    echo "instance $GCP_INSTANCE not found in $GCP_ZONE/$GCP_PROJECT — provision it first" >&2; exit 1 ;;
  *)          echo "instance is $status; not starting" ;;
esac

# SSH lags ~30-60s behind RUNNING after a cold start; poll instead of guessing.
echo "waiting for sshd..."
attempts=0
until gcloud compute ssh "$GCP_INSTANCE" "${gcloud_args[@]}" --command='true' >/dev/null 2>&1; do
  attempts=$((attempts + 1))
  if [ "$attempts" -ge 24 ]; then
    echo "sshd didn't come up after 2 minutes" >&2
    exit 1
  fi
  sleep 5
done
echo "ready"
