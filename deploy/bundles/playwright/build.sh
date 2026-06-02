#!/usr/bin/env bash
# ADR 0027: build the opt-in `playwright` RO bundle.
#
# A self-contained, glibc-linked tree: a pinned Node runtime, Microsoft's
# `@playwright/cli` (browser automation as terminal commands — built for
# coding agents that have shell access), chromium-headless-shell, and ALL
# their shared-library deps — so it runs on any glibc base image without the
# base carrying browser deps. Built inside a debian:bookworm-slim stage to
# match the glibc baseline of the standard bases.
#
# We ship the CLI (not an MCP server): the agent drives it via bash — open /
# snapshot / click / screenshot / video-start..stop — which is harness-
# agnostic (any agent that runs commands), needs no per-harness MCP config,
# and is more token-efficient. See ADR 0027 "Why not an MCP server".
#
# Layout produced (mounted at /opt/engram/browser in the guest):
#   node/                 pinned Node runtime + the @playwright/cli install
#   ms-playwright/        chromium-headless-shell
#   lib/                  collected .so deps (LD_LIBRARY_PATH target)
#   fonts/ + fonts.conf   liberation faces so text renders (not boxes)
#   cli.config.json       pins headless-shell + --no-sandbox for the daemon
#   bin/playwright-cli     wrapper: sets the runtime env, execs the real CLI
#   skills/show-your-work/ the SKILL.md (from this dir)
#
# Usage:
#   build.sh --stage <dir>          # produce the unpacked tree at <dir> (dev)
#   build.sh <out.squashfs>         # produce the tree, then pack to squashfs (CI/FC)
#
# Pins live in manifest.toml. Requires Docker; the pack path also needs mksquashfs.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Pinned versions — keep in lockstep with manifest.toml.
NODE_VERSION="${NODE_VERSION:-20.18.1}"
PLAYWRIGHT_CLI_VERSION="${PLAYWRIGHT_CLI_VERSION:-0.1.13}"

build_tree() {
    local dest="$1"
    rm -rf "$dest"
    mkdir -p "$dest"

    # Build the tree inside a glibc container, then it's left at $dest (bind
    # mount). The container runs as root (apt needs it), so it chowns /out
    # back to the invoking uid at the end — otherwise the unprivileged CI
    # runner can't clean up / pack the root-owned tree (the bug that broke
    # publish-bundles on the PR-#55 merge).
    docker run --rm \
        -e NODE_VERSION="$NODE_VERSION" \
        -e PLAYWRIGHT_CLI_VERSION="$PLAYWRIGHT_CLI_VERSION" \
        -e HOST_UID="$(id -u)" \
        -e HOST_GID="$(id -g)" \
        -v "$dest:/out" \
        -v "$here/skills:/skills-src:ro" \
        debian:bookworm-slim bash -euo pipefail -c '
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        # curl to fetch node; the rest are chromium-headless-shell runtime
        # deps `playwright install --with-deps` would pull. We install them
        # here so the ldd-walk below can collect their .so into the bundle.
        apt-get install -y -qq --no-install-recommends \
            curl ca-certificates xz-utils \
            libnss3 libnspr4 libdbus-1-3 libatk1.0-0 libatk-bridge2.0-0 \
            libcups2 libdrm2 libxkbcommon0 libxcomposite1 libxdamage1 \
            libxfixes3 libxrandr2 libgbm1 libasound2 libpango-1.0-0 \
            libcairo2 libatspi2.0-0 libxshmfence1 libx11-6 libxcb1 \
            libxext6 libexpat1 libfontconfig1 libfreetype6 fonts-liberation
        rm -rf /var/lib/apt/lists/*

        ARCH="$(dpkg --print-architecture)"   # amd64 | arm64
        case "$ARCH" in
            amd64) NODE_ARCH=x64 ;;
            arm64) NODE_ARCH=arm64 ;;
            *) echo "unsupported arch $ARCH" >&2; exit 1 ;;
        esac

        # 1) Pinned Node runtime (unpacked tarball, not apt).
        mkdir -p /out/node
        curl -fsSL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-linux-${NODE_ARCH}.tar.xz" \
            | tar -xJ -C /out/node --strip-components=1
        export PATH="/out/node/bin:$PATH"

        # 2) @playwright/cli (global → /out/node/bin/playwright-cli) +
        # chromium-headless-shell into the bundle. The CLI bundles its own
        # playwright-core; install the browser via the CLI itself, with the
        # browsers path pinned inside /out so the bundle is self-contained.
        export PLAYWRIGHT_BROWSERS_PATH=/out/ms-playwright
        npm install -g --no-audit --no-fund "@playwright/cli@${PLAYWRIGHT_CLI_VERSION}"
        playwright-cli install-browser chromium-headless-shell

        # 3) Collect every .so dep of node + the chromium-headless-shell
        # binary into /out/lib so the bundle is base-image-agnostic.
        mkdir -p /out/lib
        collect() {
            ldd "$1" 2>/dev/null | awk "/=>/ {print \$3} /ld-linux/ {print \$1}" \
                | grep -E "^/" | sort -u | while read -r so; do
                    cp -nL "$so" /out/lib/ 2>/dev/null || true
                done
        }
        collect /out/node/bin/node
        # The headless-shell binary was renamed `headless_shell` ->
        # `chrome-headless-shell` (Chrome 149 / playwright v1224+); match both
        # so we always ldd-collect chromium'"'"'s .so deps. If this finds
        # nothing the bundle ships without chromium'"'"'s libs and the browser
        # crashes at launch — fail loud instead.
        shell_bin="$(find /out/ms-playwright -type f \( -name chrome-headless-shell -o -name headless_shell \) | head -n1)"
        if [ -z "$shell_bin" ]; then
            echo "FATAL: headless-shell binary not found under /out/ms-playwright" >&2
            exit 1
        fi
        collect "$shell_bin"

        # 3b) Fonts + a minimal fontconfig. `ldd` collects libfontconfig but
        # NOT the font FILES or config, so without this chromium renders text
        # as fallback boxes — useless for the visuals this bundle exists for.
        mkdir -p /out/fonts
        cp /usr/share/fonts/truetype/liberation/*.ttf /out/fonts/ 2>/dev/null || true
        cat > /out/fonts.conf <<"FONTS"
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig>
  <dir>/opt/engram/browser/fonts</dir>
  <cachedir>/tmp/engram-fontconfig-cache</cachedir>
  <config></config>
</fontconfig>
FONTS

        # 4) CLI config: drive chromium-headless-shell, headless, no-sandbox
        # (the guest microVM is the isolation boundary). The playwright-cli
        # daemon reads this via PLAYWRIGHT_MCP_CONFIG (set by the wrapper),
        # so the agent never passes --config.
        cat > /out/cli.config.json <<"CFG"
{
  "browser": {
    "browserName": "chromium",
    "launchOptions": {
      "channel": "chromium-headless-shell",
      "headless": true,
      "args": ["--no-sandbox", "--disable-dev-shm-usage"]
    }
  }
}
CFG

        # 5) Wrapper: points the dynamic loader + Node + Playwright +
        # fontconfig + the CLI config at the bundle, then execs the real CLI.
        # agentd symlinks this onto PATH as `playwright-cli`, so the agent
        # just runs `playwright-cli open <url>` with zero flags.
        mkdir -p /out/bin
        cat > /out/bin/playwright-cli <<"WRAP"
#!/bin/sh
# ADR 0027 playwright bundle wrapper. Self-contained: runs on any glibc base
# at or above the build base glibc (see manifest.toml).
here="/opt/engram/browser"
export LD_LIBRARY_PATH="$here/lib:${LD_LIBRARY_PATH:-}"
export PLAYWRIGHT_BROWSERS_PATH="$here/ms-playwright"
export PLAYWRIGHT_MCP_CONFIG="$here/cli.config.json"
export FONTCONFIG_FILE="$here/fonts.conf"
export PATH="$here/node/bin:$PATH"
exec "$here/node/bin/playwright-cli" "$@"
WRAP
        chmod 0755 /out/bin/playwright-cli

        # 6) The show-your-work skill ships in this bundle.
        mkdir -p /out/skills
        cp -R /skills-src/show-your-work /out/skills/

        # 7) Hand the tree back to the invoking user (see build_tree comment).
        chown -R "$HOST_UID:$HOST_GID" /out
    '
}

if [[ "${1:-}" == "--stage" ]]; then
    dest="${2:?usage: build.sh --stage <dir>}"
    build_tree "$dest"
    echo "staged playwright bundle tree -> $dest"
    exit 0
fi

out="${1:?usage: build.sh <out.squashfs> | --stage <dir>}"
command -v mksquashfs >/dev/null || {
    echo "mksquashfs not found (install squashfs-tools)" >&2
    exit 1
}
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
build_tree "$tmp"
rm -f "$out"
mksquashfs "$tmp" "$out" -comp zstd -all-root -noappend -no-xattrs >/dev/null
sha="$(sha256sum "$out" | cut -d' ' -f1)"
echo "built $out"
echo "sha256: $sha"
