#!/usr/bin/env bash
# `just bake-demo` — build deploy/demo/ as a PLAIN docker image and
# push it to the LOCAL OCI registry (localhost:5001, the registry
# `just dev` runs). ADR 0024; ADR 0080 phase 3b: the engram-artifact
# bake is retired from this flow — enable-time host-side
# materialization (MaterializeImage) turns the pushed docker image
# into the chunked bootable ext4, so this script is now the exact
# user contract: `docker build && docker push`.
#
# ADR 0062: the image carries NO harness. The built-in `claude` harness is a
# per-session selection that rides the fleet `current_bundles` stamp (staged
# via `just bundles-squashfs` / `bundles-vz`), not baked into the image.
# ADR 0080: agentd is NOT in the image either — it rides the `agentd`
# bundle; the stage-1 init shim is injected at enable-time materialize.

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

REGISTRY="localhost:5001"

# Dev builds target the host's own arch (the base image is pinned via
# --platform on arm64 below so the guest binaries match the VZ/FC
# guest kernel arch).
case "$(uname -m)" in
    arm64 | aarch64) ARM=1 ;;
    x86_64 | amd64)  ARM=0 ;;
    *)
        echo "bake-demo: unsupported arch $(uname -m)" >&2
        exit 1
        ;;
esac

# Stage the image source; pin the base images to the guest arch on
# arm64 (mirrors integration-bake-demo.sh).
STAGING="./var/bake/demo"
rm -rf "$STAGING"
mkdir -p "$STAGING"
cp deploy/demo/Dockerfile "$STAGING/Dockerfile"
if [ "$ARM" = "1" ]; then
    sed -i.bak 's|^FROM |FROM --platform=linux/arm64 |' "$STAGING/Dockerfile"
    rm -f "$STAGING/Dockerfile.bak"
fi

echo "==> docker build + push demo -> $REGISTRY/demo:warm-1"
docker build -t "$REGISTRY/demo:warm-1" "$STAGING"
docker push "$REGISTRY/demo:warm-1"

echo ""
echo "✓ pushed $REGISTRY/demo:warm-1"
echo "  enable it: engram image enable --uri $REGISTRY/demo:warm-1 --config deploy/demo/image-config.toml"
echo "  (the enable materializes on a host — the host-agent needs a TAR-capable"
echo "   mke2fs (ADR 0084: libarchive-enabled) on PATH or via ENGRAM_MKE2FS."
echo "   macOS/VZ: launch tilt from 'nix develop' — Homebrew's mke2fs is built"
echo "   WITHOUT libarchive and cannot pack; dev-fc/Colima: just fc-colima-provision fc-dev)"
echo "  or create a session against --image $REGISTRY/demo:warm-1"
