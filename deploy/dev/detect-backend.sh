#!/usr/bin/env bash
# Print the sandbox backend this host can run: `firecracker`, `vz`, or
# `process`. The single source of truth for the dev-stack's
# host-capability decision (ADR 0024).
#
# Why this lives in a script and not in the binary: the backends are
# compile-time gated — `VzBackend` is `#[cfg(target_os = "macos")]` and
# Firecracker only compiles on Linux — so a single binary can't contain
# all three and can't "auto-detect" among them. Backend selection is a
# host-capability decision, so it lives one layer up (here) and the
# binary receives a concrete `ENGRAM_SANDBOX_BACKEND`.
#
# Consumers: the Tiltfile (picks backend + process topology), and the
# bake / pull-kernel helper scripts (derive the guest arch). Production
# never runs this — Helm sets ENGRAM_SANDBOX_BACKEND explicitly.
#
# Contract: exactly one word on stdout (the backend); any diagnostics go
# to stderr. Override with ENGRAM_SANDBOX_BACKEND to short-circuit the
# probe (e.g. to force `process` on a KVM box for product-plane work).

set -euo pipefail

# Honor an explicit override so a developer can pin the backend without
# editing anything — mirrors the binary's own env var name.
if [ -n "${ENGRAM_SANDBOX_BACKEND:-}" ]; then
    echo "${ENGRAM_SANDBOX_BACKEND}"
    exit 0
fi

# Rule (keep in lockstep with ADR 0024's table):
#   /dev/kvm present & readable  -> firecracker
#   else macOS && arm64          -> vz
#   else                         -> process
if [ -r /dev/kvm ]; then
    echo firecracker
    exit 0
fi

UNAME_S="$(uname -s)"
UNAME_M="$(uname -m)"
if [ "$UNAME_S" = "Darwin" ] && { [ "$UNAME_M" = "arm64" ] || [ "$UNAME_M" = "aarch64" ]; }; then
    echo vz
    exit 0
fi

# No KVM, not Apple Silicon: fall back to the un-isolated process
# backend (product-plane / web-only dev; coordinator mode=all). The
# Tiltfile sets ENGRAM_ALLOW_INSECURE_PROCESS_BACKEND=1 in that case.
echo process
