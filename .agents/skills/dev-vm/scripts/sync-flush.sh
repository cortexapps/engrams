#!/usr/bin/env bash
# Force a full reconciliation pass and block until both sides agree.
# Useful before invoking `cargo test` over there if you've just made
# many quick edits and want to be sure they all landed.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/config.sh"
mutagen sync flush "$MUTAGEN_SESSION"
