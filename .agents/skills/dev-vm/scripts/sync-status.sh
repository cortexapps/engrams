#!/usr/bin/env bash
# Show Mutagen session detail (last sync time, conflicts, problems).
# Use when something feels wrong — `Conflicts: 0 Problems: 0` is healthy.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/config.sh"

if ! mutagen sync list "$MUTAGEN_SESSION" >/dev/null 2>&1; then
  echo "no session '$MUTAGEN_SESSION' — run sync-start.sh"
  exit 0
fi
mutagen sync list "$MUTAGEN_SESSION" --long
