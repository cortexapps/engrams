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
# Layout produced (ADR 0055: mounted at a dynamic reserved slot
# `/opt/engram/dyn/<i>` — the wrapper self-locates its root from $0, so the
# bundle is position-independent and never names a fixed mount path):
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
        -v "$here/smoke.sh:/smoke.sh:ro" \
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

        # 3a) NSS PKCS#11 modules + their own deps. libnss3 (collected above)
        # dlopen'"'"'s its softoken / freebl / built-in-roots modules at runtime BY
        # NAME — they are in NO binary'"'"'s DT_NEEDED, so the ldd-walk is
        # structurally blind to them and never copies them, even though the
        # libnss3 package installed them here. Same class as the fonts below
        # (runtime data ldd can'"'"'t see). chromium only forces the NSS path when a
        # server cert chains to a PRIVATELY-added root — i.e. the egress proxy'"'"'s
        # MITM CA (ADR 0006); the built-in BoringSSL verifier handles public
        # roots without NSS, so about:blank / http / public-CA HTTPS all survive
        # and mask the gap. On that path, a missing module aborts chromium FATAL
        # in crypto/nss_util.cc during NSS init (nss_error=-5925 can'"'"'t load
        # softoken; -8023 softoken self-test fails for want of libfreeblpriv3).
        # Ship the WHOLE NSS runtime module set, then ldd-collect each so its own
        # transitive deps (softoken pulls in libsqlite3, which nothing else in
        # the bundle links) land in /out/lib too. `find` handles the amd64/arm64
        # multiarch dir (Debian ships these flat, not in an nss/ subdir). No .chk
        # files: chromium runs NSS non-FIPS, so they go unverified — and a stale
        # .chk would only risk a spurious FIPS self-test failure on an NSS bump.
        nssdir="$(dirname "$(find /usr/lib -name libsoftokn3.so 2>/dev/null | head -n1)")"
        if [ ! -e "$nssdir/libsoftokn3.so" ]; then
            echo "FATAL: NSS modules not found under /usr/lib (libnss3 not installed?)" >&2
            exit 1
        fi
        for m in libsoftokn3 libfreebl3 libfreeblpriv3 libnssckbi libnssdbm3; do
            cp -nL "$nssdir/$m.so" /out/lib/ 2>/dev/null || true
            [ -e "/out/lib/$m.so" ] && collect "/out/lib/$m.so"
        done
        [ -e /out/lib/libsoftokn3.so ] || { echo "FATAL: failed to stage libsoftokn3.so into the bundle" >&2; exit 1; }

        # 3b) Fonts + a minimal fontconfig. `ldd` collects libfontconfig but
        # NOT the font FILES or config, so without this chromium renders text
        # as fallback boxes — useless for the visuals this bundle exists for.
        mkdir -p /out/fonts
        cp /usr/share/fonts/truetype/liberation/*.ttf /out/fonts/ 2>/dev/null || true
        cat > /out/fonts.conf <<"FONTS"
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig>
  <!-- ADR 0055: the bundle mounts at a dynamic slot, so the font dir is
       resolved relative to this config file location via prefix=relative.
       The wrapper points FONTCONFIG_FILE at the bundle fonts.conf. -->
  <dir prefix="relative">fonts</dir>
  <cachedir>/tmp/engram-fontconfig-cache</cachedir>
  <config></config>
</fontconfig>
FONTS

        # 4) CLI config: drive chromium-headless-shell, headless, no-sandbox
        # (the guest microVM is the isolation boundary). The playwright-cli
        # daemon reads this via PLAYWRIGHT_MCP_CONFIG (set by the wrapper),
        # so the agent never passes --config.
        #
        # ignoreDefaultArgs [--disable-dev-shm-usage] is load-bearing.
        # Playwright ALWAYS injects --disable-dev-shm-usage into chromium
        # (a CI/Docker-small-/dev/shm safety default); config `args` are
        # appended, so it cannot be removed by omission, only by ignoring the
        # default. That flag pushes the renderer shared memory off /dev/shm
        # into the system tmpdir; the guest /tmp is not a world-writable 1777
        # dir, so the renderer shm allocation fails and the page process
        # crashes (page reset to about:blank, every navigation times out). The
        # FC guest mounts a real /dev/shm (see engram-init in
        # engram-image-builder), so we keep chromium on it and the browser
        # renders. Validated in a live prod dev_vm session (ADR 0027 e2e).
        cat > /out/cli.config.json <<"CFG"
{
  "browser": {
    "browserName": "chromium",
    "launchOptions": {
      "channel": "chromium-headless-shell",
      "headless": true,
      "args": ["--no-sandbox"],
      "ignoreDefaultArgs": ["--disable-dev-shm-usage"]
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
# ADR 0027/0055 playwright bundle wrapper. Self-contained: runs on any glibc
# base at or above the build base glibc (see manifest.toml). ADR 0055: this
# bundle mounts at a dynamic reserved slot (/opt/engram/dyn/<i>) and agentd
# symlinks this wrapper onto PATH, so we resolve our real bundle root from $0
# (readlink -f follows the PATH symlink) instead of hardcoding a mount path.
here="$(cd -- "$(dirname -- "$(readlink -f -- "$0")")/.." && pwd)"
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

        # 6b) Self-containment smoke: render a private-root HTTPS page using ONLY
        # the bundled NSS, so an incomplete lib/ (a dlopen'"'"'d NSS module or one of
        # its deps the ldd-walk missed) fails the BAKE instead of every real
        # navigation in a live session. Deletes the container'"'"'s system NSS first
        # so it cannot mask a gap — see smoke.sh. The container is throwaway.
        bash /smoke.sh /out

        # 7) Hand the tree back to the invoking user (see build_tree comment).
        chown -R "$HOST_UID:$HOST_GID" /out
    '
}

if [[ "${1:-}" == "--stage" ]]; then
    dest="${2:?usage: build.sh --stage <dir>}"
    build_tree "$dest"
    cp "$here/mount.json" "$dest/"  # ADR 0055: activate() reads this
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
cp "$here/mount.json" "$tmp/"  # ADR 0055: activate() reads this
# Reproducible content-addressed pack (ADR 0027/0035): identical content MUST
# yield an identical sha across builds — see deploy/bundles/_pack.sh.
# shellcheck source=../_pack.sh
. "$(dirname "${BASH_SOURCE[0]}")/../_pack.sh"
pack_squashfs "$tmp" "$out"
sha="$(sha256sum "$out" | cut -d' ' -f1)"
echo "built $out"
echo "sha256: $sha"
