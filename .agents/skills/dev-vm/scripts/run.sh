#!/usr/bin/env bash
# Run a command on the VM, inside the synced repo, inside the nix shell.
# Both invocation styles work:
#   run.sh cargo check --workspace                      # argv form
#   run.sh just check
#   run.sh nproc
#   run.sh 'cargo clippy --workspace -- -D warnings'    # whole line quoted
#   run.sh 'a && b | c'                                 # shell operators need this form
#
# Sync state is sanity-checked first so you don't run against a stale tree.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/config.sh"

if [ $# -eq 0 ]; then
  echo "usage: run.sh <cmd> [args...]" >&2
  exit 2
fi

# Best-effort sync flush. If mutagen isn't running we just trust the
# user — they may have intentionally rsync'd or scp'd something.
if command -v mutagen >/dev/null 2>&1 && mutagen sync list "$MUTAGEN_SESSION" >/dev/null 2>&1; then
  mutagen sync flush "$MUTAGEN_SESSION" >/dev/null 2>&1 || true
fi

# Two ways callers pass a command, both supported:
#
#   * argv form — multiple args, or a single bare word: `run.sh cargo check`,
#     `run.sh nproc`. We `%q`-quote each element so it round-trips as argv and
#     hand it straight to `nix develop --command` (nix takes cmd + args itself).
#
#   * command-line form — a single arg containing whitespace:
#     `run.sh 'cargo clippy -- -D warnings'`, `run.sh 'a && b'`. Here the one
#     string IS the command line, so we run it through `bash -c` rather than
#     treating "cargo clippy ..." as a single (nonexistent) program name. This
#     is the only form that supports shell operators (&&, |, redirects).
#
# Without this split, a quoted whole-line invocation fails with
# `exec: <the whole string>: not found` (nix tries to exec it as one binary).
if [ $# -eq 1 ] && [[ $1 == *[[:space:]]* ]]; then
  exec gcloud compute ssh "$GCP_INSTANCE" "${gcloud_args[@]}" \
    --command="cd ${REMOTE_DIR} && /nix/var/nix/profiles/default/bin/nix develop --command bash -c $(printf '%q' "$1")"
else
  quoted=$(printf '%q ' "$@")
  exec gcloud compute ssh "$GCP_INSTANCE" "${gcloud_args[@]}" \
    --command="cd ${REMOTE_DIR} && /nix/var/nix/profiles/default/bin/nix develop --command ${quoted}"
fi
