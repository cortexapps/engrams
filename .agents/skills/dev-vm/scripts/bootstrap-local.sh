#!/usr/bin/env bash
# One-time local setup so the rest of the scripts have everything they
# need: mutagen for sync, ~/.ssh/config entries gcloud writes for the VM.
# Idempotent — re-run any time.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/config.sh"

echo "=== gcloud ==="
if ! command -v gcloud >/dev/null 2>&1; then
  echo "gcloud not on PATH; install Google Cloud SDK first" >&2
  exit 1
fi

echo "=== mutagen ==="
if ! command -v mutagen >/dev/null 2>&1; then
  if ! command -v brew >/dev/null 2>&1; then
    echo "no brew; install mutagen manually: https://mutagen.io/documentation/introduction/installation" >&2
    exit 1
  fi
  brew install mutagen-io/mutagen/mutagen
fi
mutagen --version

echo "=== gcloud SSH config ==="
# Writes ~/.ssh/config entries for every instance in the project so plain
# `ssh <alias>` (and mutagen, rsync, ...) works without going through gcloud
# every time. Re-run safely; gcloud only updates its own managed block.
gcloud compute config-ssh --project="$GCP_PROJECT" --quiet | tail -3

echo "=== User override (OS Login) ==="
# gcloud's auto-generated block doesn't set `User`, so plain `ssh <alias>`
# defaults to the local Mac username — which doesn't exist on the VM under
# OS Login. We prepend our own Host block with the correct OS Login
# username; SSH evaluates first-match-wins, so this takes precedence.
osl_user="$(gcloud compute os-login describe-profile --format='value(posixAccounts[0].username)' 2>/dev/null || true)"
if [ -z "$osl_user" ]; then
  echo "could not look up OS Login username — falling back to local user (may fail)" >&2
else
  ssh_config="$HOME/.ssh/config"
  marker_begin="# BEGIN engram-dev-vm (User override)"
  marker_end="# END engram-dev-vm"
  if [ -f "$ssh_config" ] && grep -q "$marker_begin" "$ssh_config"; then
    # Replace the existing block in-place.
    awk -v mb="$marker_begin" -v me="$marker_end" '
      $0==mb { skip=1; next }
      $0==me { skip=0; next }
      !skip
    ' "$ssh_config" > "$ssh_config.tmp"
    mv "$ssh_config.tmp" "$ssh_config"
  fi
  {
    echo "$marker_begin"
    echo "Host $SSH_HOST"
    echo "    User $osl_user"
    echo "$marker_end"
    echo
    [ -f "$ssh_config" ] && cat "$ssh_config"
  } > "$ssh_config.new"
  mv "$ssh_config.new" "$ssh_config"
  chmod 600 "$ssh_config"
  echo "User=$osl_user pinned for $SSH_HOST"
fi

echo "=== alias resolved? ==="
resolved_user="$(ssh -G "$SSH_HOST" 2>/dev/null | awk '$1=="user"{print $2}')"
if [ -z "$resolved_user" ]; then
  echo "ssh alias '$SSH_HOST' did not resolve — check ~/.ssh/config" >&2
  exit 1
fi
echo "ssh alias '$SSH_HOST' resolves; user=$resolved_user"

echo "DONE"
