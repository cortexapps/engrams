#!/usr/bin/env bash
# `just bake-demo` — build the Claude harness from source, publish it to
# the LOCAL OCI registry, and bake deploy/demo-claude/ against it,
# pushing the image to localhost:5001 (the registry `just dev` runs).
# ADR 0024. Switch-free: detect-backend.sh + `uname` decide the arch.
#
# "Build locally, not from GHCR": the default catalog points
# harness-claude at GHCR with a pinned, released tag. For the inner loop
# we instead build engram-harness-claude from the working tree, publish
# it to localhost:5001, and point the baker at it via
# ENGRAM_BUILTIN_HARNESS_CLAUDE_REPO — so you bake what you have. This is
# the exact mechanism CI's test-e2e-stack lane uses, minus GHCR.

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
# Local mirror of the GHCR repo path; the version MUST match
# deploy/demo-claude/engram.toml's [harness] version (the baker resolves
# `<repo>:<version>-<platform>` and pulls exactly that).
HARNESS_REPO_PATH="cortexapps/engrams/harness-claude"
HARNESS_VERSION="v0.1.0"

# Dev bakes cross-compile the rootfs for the host's own arch, so guest
# arch == host arch. Transport depends on the backend (VZ uses
# virtio-console; Firecracker/process use vsock).
case "$(uname -m)" in
    arm64 | aarch64)
        TARGET=aarch64-unknown-linux-musl
        HARNESS_PLATFORM=linux-arm64
        CLAUDE_PLAT=linux-arm64
        ARM=1
        ;;
    x86_64 | amd64)
        TARGET=x86_64-unknown-linux-musl
        HARNESS_PLATFORM=linux-x86_64
        CLAUDE_PLAT=linux-x64
        ARM=0
        ;;
    *)
        echo "bake-demo: unsupported arch $(uname -m)" >&2
        exit 1
        ;;
esac
if [ "$backend" = "vz" ]; then TRANSPORT=console; else TRANSPORT=vsock; fi

rustup target add "$TARGET" >/dev/null 2>&1 || true

echo "==> build agentd + harness-claude ($TARGET) + cli + publisher"
cargo build --release --target "$TARGET" -p engram-agentd -p engram-harness-claude
cargo build --release -p engram-cli -p engram-publish-builtin-harness

# Bundle the matching Claude Code CLI (cached across runs).
CLAUDE_VERSION="$(curl -fsSL https://downloads.claude.ai/claude-code-releases/latest)"
CACHE="$HOME/.cache/engram-claude-cli/$CLAUDE_VERSION/$CLAUDE_PLAT"
if [ ! -x "$CACHE/claude" ]; then
    echo "==> download claude CLI $CLAUDE_VERSION ($CLAUDE_PLAT)"
    mkdir -p "$CACHE"
    curl -fsSL --retry 3 -o "$CACHE/claude.tmp" \
        "https://downloads.claude.ai/claude-code-releases/$CLAUDE_VERSION/$CLAUDE_PLAT/claude"
    chmod +x "$CACHE/claude.tmp"
    mv "$CACHE/claude.tmp" "$CACHE/claude"
fi

# Stage the artifact (== contents of /opt/engram/harness/) and publish.
STAGE="$(mktemp -d)"
cp -p "target/$TARGET/release/engram-harness-claude" "$STAGE/harness"
cp -p "$CACHE/claude" "$STAGE/claude"
cat >"$STAGE/artifact.toml" <<EOF
name = "claude"
version = "$HARNESS_VERSION"
entry = "harness"
description = "Anthropic Claude Code; locally-built engram-harness-claude $HARNESS_VERSION on claude CLI $CLAUDE_VERSION."
EOF

HARNESS_URI="$REGISTRY/$HARNESS_REPO_PATH:$HARNESS_VERSION-$HARNESS_PLATFORM"
echo "==> publish harness artifact -> $HARNESS_URI"
./target/release/engram-publish-builtin-harness --from "$STAGE" --to "$HARNESS_URI"
rm -rf "$STAGE"

# Stage the image source; pin the debian base to the guest arch on
# arm64 so the rootfs binaries match the kernel (mirrors vz-bake-demo).
STAGING="./var/bake/demo-claude"
rm -rf "$STAGING"
mkdir -p "$STAGING"
cp deploy/demo-claude/Dockerfile "$STAGING/Dockerfile"
cp deploy/demo-claude/engram.toml "$STAGING/engram.toml"
if [ "$ARM" = "1" ]; then
    sed -i.bak 's|^FROM debian:|FROM --platform=linux/arm64 debian:|' "$STAGING/Dockerfile"
    rm -f "$STAGING/Dockerfile.bak"
fi

echo "==> bake demo-claude -> $REGISTRY/demo-claude:warm-1 (harness $HARNESS_PLATFORM)"
# ENGRAM_BUILTIN_HARNESS_CLAUDE_REPO points the baker's catalog at the
# artifact we just published locally.
ENGRAM_BUILTIN_HARNESS_CLAUDE_REPO="$REGISTRY/$HARNESS_REPO_PATH" \
    ./target/release/engram-cli image build \
    --repo demo-claude \
    --tag warm-1 \
    --source "$STAGING" \
    --format ext4 \
    --images-dir ./var/bake/_staging \
    --transport "$TRANSPORT" \
    --harness-platform "$HARNESS_PLATFORM" \
    --inject-agent "target/$TARGET/release/engram-agentd" \
    --push "$REGISTRY/demo-claude"

echo ""
echo "✓ pushed $REGISTRY/demo-claude:warm-1"
echo "  enable it: engram image enable $REGISTRY/demo-claude:warm-1"
echo "  or create a session against --image $REGISTRY/demo-claude:warm-1"
