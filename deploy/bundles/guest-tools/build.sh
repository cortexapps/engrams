#!/usr/bin/env bash
# ADR 0080 §D: build the fleet-wide `guest-tools` RO bundle — engrams-owned
# in-guest tooling that no longer bakes into session images. Today it carries
# exactly one tool: the static `ttyd` binary agentd's `shell.rs` lazily spawns
# for the dashboard SHELL tab (agentd probes the dyn mounts for `ttyd` at
# StartShell time; the image contract is down to "/bin/sh only").
#
# ttyd is PINNED — version + per-arch sha256 — from tsl0922's GitHub release
# binaries (fully static, verified `statically linked` for both arches). The
# download is verified against the pinned sha before packing, so the bundle's
# content-address is a pure function of this file. Bump deliberately:
# re-verify BOTH shas and smoke the SHELL tab (prod hit a 1.7.7-release-page
# segfault once — see docs/history; 1.7.7 release binaries are the ones that
# work, the crash was in a Docker-Hub 1.7.7 image build).
#
# Rides the host `current_bundles` stamp under `guest-tools` (reserved slot
# dyn_2). Fresh creates get the fleet's current generation patch_drive'd in;
# a ttyd bump ships by republishing this bundle — ZERO image re-bakes, ZERO
# base-snapshot recaptures (the same delivery as agentd/harness).
#
# Usage:
#   build.sh <out.squashfs> [arch]   # pack to squashfs (CI / FC host bake)
#   build.sh --stage <dir> [arch]    # stage the unpacked tree (dev / erofs)
#
# `arch` defaults to the host arch (FC/VZ guests match the host). Prints the
# sha256 of the squashfs on the pack path.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

TTYD_VERSION="1.7.7"
TTYD_SHA256_X86_64="8a217c968aba172e0dbf3f34447218dc015bc4d5e59bf51db2f2cd12b7be4f55"
TTYD_SHA256_AARCH64="b38acadd89d1d396a0f5649aa52c539edbad07f4bc7348b27b4f4b7219dd4165"

sha256_of() {
    if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1;
    else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

resolve_arch() {
    local arch="${1:-$(uname -m)}"
    case "$arch" in
        arm64 | aarch64) echo "aarch64" ;;
        x86_64 | amd64)  echo "x86_64" ;;
        *)
            echo "guest-tools: unsupported arch $arch" >&2
            return 1
            ;;
    esac
}

stage_tree() { # stage_tree <dest> <arch>
    local dest="$1" arch="$2" want got
    case "$arch" in
        x86_64)  want="$TTYD_SHA256_X86_64" ;;
        aarch64) want="$TTYD_SHA256_AARCH64" ;;
    esac
    mkdir -p "$dest"
    echo "==> ttyd ${TTYD_VERSION} (${arch}) from the pinned GitHub release" >&2
    curl -fsSL --retry 3 --retry-delay 2 \
        "https://github.com/tsl0922/ttyd/releases/download/${TTYD_VERSION}/ttyd.${arch}" \
        -o "$dest/ttyd"
    got="$(sha256_of "$dest/ttyd")"
    if [[ "$got" != "$want" ]]; then
        echo "guest-tools: ttyd.${arch} sha256 mismatch: got $got want $want" >&2
        rm -f "$dest/ttyd"
        return 1
    fi
    chmod 0755 "$dest/ttyd"
}

if [[ "${1:-}" == "--stage" ]]; then
    dest="${2:?usage: build.sh --stage <dir> [arch]}"
    arch="$(resolve_arch "${3:-}")"
    rm -rf "$dest"
    stage_tree "$dest" "$arch"
    echo "staged guest-tools bundle tree -> $dest"
    exit 0
fi

out="${1:?usage: build.sh <out.squashfs> [arch] | --stage <dir> [arch]}"
arch="$(resolve_arch "${2:-}")"
command -v mksquashfs >/dev/null || {
    echo "mksquashfs not found (install squashfs-tools)" >&2
    exit 1
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
stage_tree "$tmp" "$arch"

# Reproducible content-addressed pack (ADR 0027/0035): identical content MUST
# yield an identical sha across builds — see deploy/bundles/_pack.sh.
# shellcheck source=../_pack.sh
. "$here/../_pack.sh"
pack_squashfs "$tmp" "$out"

sha="$(sha256sum "$out" | cut -d' ' -f1)"
echo "built $out"
echo "sha256: $sha"
