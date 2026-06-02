#!/usr/bin/env bash
# ADR 0027: build the opt-in `playwright` RO bundle.
#
# A self-contained, glibc-linked tree: a pinned Node runtime, the
# @playwright/mcp server, chromium-headless-shell, and ALL their shared-
# library deps — so it runs on any glibc base image without the base
# carrying browser deps. Built inside a debian:bookworm-slim stage to match
# the glibc baseline of the standard bases.
#
# Layout produced (mounted at /opt/engram/browser in the guest):
#   node/                 pinned Node runtime
#   node_modules/         @playwright/mcp + deps
#   ms-playwright/        chromium-headless-shell
#   lib/                  collected .so deps (LD_LIBRARY_PATH target)
#   launch-mcp            launcher (sets LD_LIBRARY_PATH/PLAYWRIGHT_*; execs node cli.js)
#   skills/record-demo/   the record-demo SKILL.md (from this dir)
#
# Usage:
#   build.sh --stage <dir>          # produce the unpacked tree at <dir> (dev)
#   build.sh <out.squashfs>         # produce the tree, then pack to squashfs (CI/FC)
#
# Pins live in manifest.toml (asserted by CI). Requires Docker; the pack
# path additionally requires mksquashfs.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Pinned versions — keep in lockstep with manifest.toml.
NODE_VERSION="${NODE_VERSION:-20.18.1}"
PLAYWRIGHT_MCP_VERSION="${PLAYWRIGHT_MCP_VERSION:-0.0.41}"

build_tree() {
    local dest="$1"
    rm -rf "$dest"
    mkdir -p "$dest"

    # Build the tree inside a glibc container, then copy it out. The
    # heredoc script runs as root in debian:bookworm-slim.
    local cid
    cid="$(docker create debian:bookworm-slim sleep infinity 2>/dev/null || true)"

    docker run --rm \
        -e NODE_VERSION="$NODE_VERSION" \
        -e PLAYWRIGHT_MCP_VERSION="$PLAYWRIGHT_MCP_VERSION" \
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

        # 2) @playwright/mcp + chromium-headless-shell, browsers inside /out.
        export PLAYWRIGHT_BROWSERS_PATH=/out/ms-playwright
        cd /out
        npm install --no-save --no-package-lock "@playwright/mcp@${PLAYWRIGHT_MCP_VERSION}"
        ./node_modules/.bin/playwright install chromium-headless-shell

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
        shell_bin="$(find /out/ms-playwright -name headless_shell -type f | head -n1)"
        [ -n "$shell_bin" ] && collect "$shell_bin"

        # 3b) Fonts + a minimal fontconfig. `ldd` collects libfontconfig but
        # NOT the font FILES or config, so without this chromium renders text
        # as fallback boxes (`Fontconfig error: Cannot load default config`)
        # — useless for the visual demos this bundle exists for. Ship the
        # liberation faces + a self-contained fonts.conf that points at them.
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

        # 4) Launcher: sets the runtime env, then execs the MCP cli.
        cat > /out/launch-mcp <<"LAUNCH"
#!/bin/sh
# ADR 0027 playwright bundle launcher. Self-contained: points the dynamic
# loader + Node + Playwright + fontconfig at the bundle so it runs on any
# glibc base at or above the build base glibc (see manifest.toml).
here="/opt/engram/browser"
export LD_LIBRARY_PATH="$here/lib:${LD_LIBRARY_PATH:-}"
export PLAYWRIGHT_BROWSERS_PATH="$here/ms-playwright"
export FONTCONFIG_FILE="$here/fonts.conf"
export PATH="$here/node/bin:$PATH"
exec "$here/node/bin/node" "$here/node_modules/@playwright/mcp/cli.js" "$@"
LAUNCH
        chmod 0755 /out/launch-mcp

        # 5) The record-demo skill ships in this bundle.
        mkdir -p /out/skills
        cp -R /skills-src/record-demo /out/skills/
    '
    [ -n "$cid" ] && docker rm -f "$cid" >/dev/null 2>&1 || true
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
