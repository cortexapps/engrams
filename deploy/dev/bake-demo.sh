#!/usr/bin/env bash
# `just bake-demo` — bake deploy/demo/ and push it to the LOCAL OCI
# registry (localhost:5001, the registry `just dev` runs). ADR 0024.
# Switch-free: detect-backend.sh + `uname` decide the arch.
#
# ADR 0062: the image carries NO harness. The built-in `claude` harness is a
# per-session selection that rides the fleet `current_bundles` stamp (staged
# via `just bundles-squashfs` / `bundles-vz`), not baked into the image — so
# this script only bakes the rootfs + agentd.

set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

: "${ENGRAM_KEK_MASTER_KEY:?run \`just bootstrap\` first to generate a KEK}"

# Detect the sandbox backend (once, reused below).
backend="$(bash deploy/dev/detect-backend.sh)"

# On macOS/VZ, check that e2fsprogs (mke2fs) is available for ext4 image building.
# (Registry pushes require ext4 format; directory format is local-only.)
if [ "$backend" = "vz" ]; then
    PATH="/opt/homebrew/opt/e2fsprogs/sbin:$PATH"
    if ! command -v mke2fs >/dev/null 2>&1; then
        echo "Error: mke2fs not found. VZ image builds require e2fsprogs." >&2
        echo "" >&2
        echo "Install it with:" >&2
        echo "  brew install e2fsprogs" >&2
        exit 1
    fi
fi

REGISTRY="localhost:5001"

# Dev bakes cross-compile the rootfs for the host's own arch. Every
# backend uses the vsock transport (ADR 0066 Phase 2 migrated VZ off
# virtio-console onto real vsock).
case "$(uname -m)" in
    arm64 | aarch64)
        TARGET=aarch64-unknown-linux-musl
        ARM=1
        ;;
    x86_64 | amd64)
        TARGET=x86_64-unknown-linux-musl
        ARM=0
        ;;
    *)
        echo "bake-demo: unsupported arch $(uname -m)" >&2
        exit 1
        ;;
esac
# All backends now use the vsock transport. VZ migrated off the
# virtio-console bridge onto Apple's real VZVirtioSocketDevice in ADR
# 0066 Phase 2 — the Kata guest kernel VZ boots ships
# CONFIG_VIRTIO_VSOCKETS=y, so vsock works there just like it does on
# Firecracker (and it muxes concurrent streams per port, so the port
# relay is head-of-line-free).
TRANSPORT=vsock

rustup target add "$TARGET" >/dev/null 2>&1 || true

echo "==> build agentd ($TARGET) + cli"
cargo build --release --target "$TARGET" -p engram-agentd
cargo build --release -p engram-cli

# Stage the image source; pin the debian base to the guest arch on
# arm64 so the rootfs binaries match the kernel (mirrors vz-bake-demo).
STAGING="./var/bake/demo"
rm -rf "$STAGING"
mkdir -p "$STAGING"
cp deploy/demo/Dockerfile "$STAGING/Dockerfile"
cp deploy/demo/engram.toml "$STAGING/engram.toml"
if [ "$ARM" = "1" ]; then
    sed -i.bak 's|^FROM debian:|FROM --platform=linux/arm64 debian:|' "$STAGING/Dockerfile"
    rm -f "$STAGING/Dockerfile.bak"
fi

echo "==> bake demo -> $REGISTRY/demo:warm-1"
./target/release/engram-cli image build \
    --repo demo \
    --tag warm-1 \
    --source "$STAGING" \
    --format ext4 \
    --images-dir ./var/bake/_staging \
    --transport "$TRANSPORT" \
    --inject-agent "target/$TARGET/release/engram-agentd" \
    --push "$REGISTRY/demo"

echo ""
echo "✓ pushed $REGISTRY/demo:warm-1"
echo "  enable it: engram image enable $REGISTRY/demo:warm-1"
echo "  or create a session against --image $REGISTRY/demo:warm-1"
