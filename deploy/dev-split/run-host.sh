#!/usr/bin/env bash
# Launch the host-agent in --mode=host. Dials the coord over WS,
# wraps the local FC backend, shares the same blob root.
#
# Runs under sudo so the FC backend's `ip tuntap` / iptables calls
# succeed; the binary itself is fully linked (no nix-shell needed at
# runtime) so we don't have to drag $HOME or the nix profile in.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
set -a
source .env
set +a
KERNEL="${ENGRAM_KERNEL_IMAGE_PATH:-$HOME/.cache/engram-fc-test/vmlinux-5.10.223}"
exec sudo \
  ENGRAM_KEK_MASTER_KEY="$ENGRAM_KEK_MASTER_KEY" \
  ENGRAM_COORDINATOR_ENDPOINT=ws://127.0.0.1:8090 \
  ENGRAM_COORDINATOR_TOKEN=dev-split-token \
  ENGRAM_SANDBOX_BACKEND=firecracker \
  ENGRAM_SANDBOX_WORK_DIR=./var/sandboxes \
  ENGRAM_LOCAL_PATH=./var/engram \
  ENGRAM_BLOB_BACKEND=local \
  ENGRAM_KERNEL_IMAGE_PATH="$KERNEL" \
  RUST_LOG=info,engram=debug \
  ./target/debug/engram-host-agent
