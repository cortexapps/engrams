#!/usr/bin/env bash
# Tear down the Mutagen sync session. Files on both sides stay put;
# subsequent edits just stop propagating until sync-start runs again.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/config.sh"

if ! mutagen sync list "$MUTAGEN_SESSION" >/dev/null 2>&1; then
  echo "no session '$MUTAGEN_SESSION'"
  exit 0
fi
mutagen sync terminate "$MUTAGEN_SESSION"
