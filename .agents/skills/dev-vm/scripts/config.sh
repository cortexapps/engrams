#!/usr/bin/env bash
# Sourced by every other script. Single source of truth for VM identity
# and the local/remote tree paths. Override via env if needed.

: "${GCP_PROJECT:=cortex-test-1608327238078}"
: "${GCP_ZONE:=us-west2-a}"
: "${GCP_INSTANCE:=engram-dev}"
: "${REMOTE_DIR:=/home/nikhil_unni_cortex_io/engrams}"
: "${MUTAGEN_SESSION:=engram}"
export GCP_PROJECT GCP_ZONE GCP_INSTANCE REMOTE_DIR MUTAGEN_SESSION

# Local tree to sync. Defaults to the main checkout (4 levels up from this
# script). To sync a GIT WORKTREE instead (e.g. .claude/worktrees/<name>),
# set ENGRAM_DEV_LOCAL_DIR to its path — only one dev-vm session runs at a
# time, so repointing is fine. If unset, auto-detect the git
# worktree/checkout containing $PWD, falling back to the main checkout.
MAIN_CHECKOUT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../../.." && pwd)"
if [ -n "${ENGRAM_DEV_LOCAL_DIR:-}" ]; then
  LOCAL_DIR="$ENGRAM_DEV_LOCAL_DIR"
else
  LOCAL_DIR="$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || true)"
  [ -z "$LOCAL_DIR" ] && LOCAL_DIR="$MAIN_CHECKOUT"
fi
export LOCAL_DIR MAIN_CHECKOUT

# A git worktree's `.git` is a gitdir-pointer FILE (not a dir). Syncing it
# would aim VM-side git at a nonexistent Mac path, so worktree mode
# excludes it: the VM builds/tests/clippy fine without git — do git ops on
# the Mac (the skill already advises this). The main checkout's `.git` is a
# real dir and stays synced.
if [ -f "$LOCAL_DIR/.git" ]; then IS_WORKTREE=1; else IS_WORKTREE=0; fi
export IS_WORKTREE

# After `gcloud compute config-ssh`, the box is reachable as this host
# alias from any tool that reads ~/.ssh/config (mutagen, plain ssh, rsync).
SSH_HOST="${GCP_INSTANCE}.${GCP_ZONE}.${GCP_PROJECT}"
export SSH_HOST

gcloud_args=(--zone="$GCP_ZONE" --project="$GCP_PROJECT")
export gcloud_args
