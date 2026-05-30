#!/usr/bin/env bash
# `just pull-kernel` — fetch the kernel artifact for whatever backend
# this host runs (ADR 0024). Switch-free: detect-backend.sh decides.
#   vz          -> Kata static arm64 kernel (VZ)
#   firecracker -> FC test kernel + rootfs
#   process     -> no kernel needed

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

backend="$(bash deploy/dev/detect-backend.sh)"
case "$backend" in
    vz)
        echo "==> VZ backend: pulling the Kata arm64 kernel"
        bash crates/engram-sandbox-vz/scripts/pull-kernel.sh
        ;;
    firecracker)
        echo "==> Firecracker backend: fetching the test kernel + rootfs"
        bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh
        ;;
    process)
        echo "process backend boots no kernel — nothing to fetch."
        ;;
    *)
        echo "pull-kernel: unknown backend '$backend'" >&2
        exit 1
        ;;
esac
