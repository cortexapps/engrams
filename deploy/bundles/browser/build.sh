#!/usr/bin/env bash
# ADR 0064: build the opt-in `browser` RO bundle.
#
# A self-contained glibc tree: chromium (full UI build), Xvfb, x11vnc, openbox,
# liberation fonts, and ALL their shared-library deps — so it runs on any glibc
# base image without the base carrying browser deps. Built inside a
# debian:bookworm-slim stage to match the glibc baseline of the standard bases.
#
# Layout produced (ADR 0055: mounted at a dynamic reserved slot
# /opt/engram/dyn/<i>; the launcher self-locates from $0):
#   chrome/      chromium binary + resources
#   bin/         Xvfb, x11vnc, openbox + the engram-browser launcher
#   lib/         collected .so deps (LD_LIBRARY_PATH target)
#   fonts/ + fonts.conf
#   mount.json   (activate() reads this; declares the engram-browser bin)
#
# Usage:
#   build.sh --stage <dir>        # unpacked tree at <dir> (dev/ProcessBackend)
#   build.sh <out.squashfs>       # tree, then pack to squashfs (CI/FC)
# Requires Docker; the pack path also needs mksquashfs.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

build_tree() {
    local dest="$1"
    rm -rf "$dest"; mkdir -p "$dest"
    docker run --rm \
        -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
        -v "$dest:/out" \
        -v "$here/bin/engram-browser:/launcher:ro" \
        debian:bookworm-slim bash -euo pipefail -c '
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq
        apt-get install -y -qq --no-install-recommends \
            chromium xvfb x11vnc openbox fonts-liberation ca-certificates
        rm -rf /var/lib/apt/lists/*

        mkdir -p /out/chrome /out/bin /out/lib /out/fonts

        # Chromium binary + its resource files (icudtl.dat, *.pak, locales).
        chrome_bin="$(command -v chromium)"
        # chromium is usually a wrapper; resolve the real binary dir.
        real_dir="/usr/lib/chromium"
        cp -aL "$real_dir/." /out/chrome/ 2>/dev/null || true
        # Ensure the launch target exists at chrome/chrome. On Debian bookworm
        # the real ELF is named `chromium` not `chrome`; prefer a symlink so
        # the launcher ($here/chrome/chrome) resolves to the real binary.
        if [ ! -x /out/chrome/chrome ]; then
            if [ -x /out/chrome/chromium ]; then
                ln -sf chromium /out/chrome/chrome
            else
                cp -L "$(readlink -f "$chrome_bin")" /out/chrome/chrome
            fi
        fi

        cp -L "$(command -v Xvfb)"   /out/bin/Xvfb
        cp -L "$(command -v x11vnc)" /out/bin/x11vnc
        cp -L "$(command -v openbox)" /out/bin/openbox
        cp /launcher /out/bin/engram-browser
        chmod 0755 /out/bin/*

        # Collect every .so dep of the binaries into /out/lib so the bundle is
        # base-image-agnostic (same ldd-walk the playwright bundle uses).
        collect() {
            ldd "$1" 2>/dev/null | awk "/=>/ {print \$3} /ld-linux/ {print \$1}" \
                | grep -E "^/" | sort -u | while read -r so; do
                    cp -nL "$so" /out/lib/ 2>/dev/null || true
                done
        }
        collect /out/chrome/chrome
        collect /out/bin/Xvfb
        collect /out/bin/x11vnc
        collect /out/bin/openbox

        cp /usr/share/fonts/truetype/liberation/*.ttf /out/fonts/ 2>/dev/null || true
        cat > /out/fonts.conf <<"FONTS"
<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "fonts.dtd">
<fontconfig>
  <dir prefix="relative">fonts</dir>
  <cachedir>/tmp/engram-browser-fontconfig</cachedir>
  <config></config>
</fontconfig>
FONTS

        chown -R "$HOST_UID:$HOST_GID" /out
    '
    cp "$here/mount.json" "$dest/"
}

if [[ "${1:-}" == "--stage" ]]; then
    dest="${2:?usage: build.sh --stage <dir>}"
    build_tree "$dest"
    echo "staged browser bundle tree -> $dest"
    exit 0
fi

out="${1:?usage: build.sh <out.squashfs> | --stage <dir>}"
command -v mksquashfs >/dev/null || { echo "mksquashfs not found (install squashfs-tools)" >&2; exit 1; }
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
build_tree "$tmp"
rm -f "$out"
mksquashfs "$tmp" "$out" -comp zstd -all-root -noappend -no-xattrs >/dev/null
echo "built $out"
echo "sha256: $(sha256sum "$out" | cut -d' ' -f1)"
