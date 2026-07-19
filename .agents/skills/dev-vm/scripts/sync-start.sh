#!/usr/bin/env bash
# Start the Mutagen sync session: local repo <-> $REMOTE_DIR on the VM.
# Two-way-resolved: both sides editable, conflicts resolved in favour of
# the more recent change.
#
# Excludes target/, node_modules/, var/, .sqlx/, .direnv/, result*,
# .DS_Store. node_modules is excluded because it holds platform-specific
# native binaries (e.g. @rollup/rollup-darwin-arm64 on the Mac vs the
# linux-x64 build on the VM) — syncing it two-way corrupts both, so each
# side runs its own `pnpm install`.
#
# `LOCAL_DIR` (from config.sh) is the main checkout OR a git worktree (set
# ENGRAM_DEV_LOCAL_DIR / auto-detected from $PWD). In MAIN-checkout mode
# `.git` is synced so `git status` works on either side. In WORKTREE mode
# (IS_WORKTREE=1) `.git` is excluded — a worktree's `.git` is a
# gitdir-pointer file that would break VM-side git — so do git ops on the
# Mac. Switching LOCAL_DIR repoints the session (terminate + recreate).
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/config.sh"

if ! command -v mutagen >/dev/null 2>&1; then
  echo "mutagen not installed; run bootstrap-local.sh first" >&2
  exit 1
fi

if mutagen sync list "$MUTAGEN_SESSION" >/dev/null 2>&1; then
  existing_alpha="$(mutagen sync list "$MUTAGEN_SESSION" 2>/dev/null \
    | awk '/^Alpha:/{f=1} f&&/URL:/{print $NF; exit}')"
  if [ "$existing_alpha" = "$LOCAL_DIR" ]; then
    echo "session '$MUTAGEN_SESSION' already syncing $LOCAL_DIR; resuming"
    mutagen sync resume "$MUTAGEN_SESSION" || true
    mutagen sync flush "$MUTAGEN_SESSION"
    exit 0
  fi
  echo "session '$MUTAGEN_SESSION' is syncing a different tree; repointing:"
  echo "  was: ${existing_alpha:-<unknown>}"
  echo "  now: $LOCAL_DIR"
  mutagen sync terminate "$MUTAGEN_SESSION" || true
fi

# Make sure the SSH alias is wired up (mutagen connects via plain ssh).
if ! ssh -G "$SSH_HOST" >/dev/null 2>&1; then
  echo "ssh alias '$SSH_HOST' missing — run bootstrap-local.sh" >&2
  exit 1
fi

# `.claude/` is excluded: it's the user's local Claude config + skills +
# nested git worktrees (hundreds of MB, and the dev-vm skill itself runs on
# the Mac). The VM never needs it, and syncing it dragged every sibling
# worktree's tree onto the VM.
# .env excluded: it carries machine-local auth config (e.g. OIDC client
# creds for web testing) that flips the DEV coordinator into OIDC mode and
# 401s API-driven runs on the VM. Each side keeps its own .env.
ignore='target/,node_modules/,var/,.sqlx/,.direnv/,result,result-*,.DS_Store,*.swp,.claude/,.env'
if [ "${IS_WORKTREE:-0}" = "1" ]; then
  ignore="${ignore},.git"
  echo "worktree mode: syncing $LOCAL_DIR (excluding .git — VM-side git disabled; build/test only)"
else
  echo "main-checkout mode: syncing $LOCAL_DIR (incl. .git)"
fi

mutagen sync create \
  --name="$MUTAGEN_SESSION" \
  --mode=two-way-resolved \
  --ignore-vcs=false \
  --ignore="$ignore" \
  "$LOCAL_DIR" \
  "${SSH_HOST}:${REMOTE_DIR}"

echo "waiting for first reconciliation..."
mutagen sync flush "$MUTAGEN_SESSION"
mutagen sync list "$MUTAGEN_SESSION"
