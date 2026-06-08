#!/usr/bin/env bash
# ADR 0044 K2 / GAP 2: stage the node assets (firecracker + the engram guest
# kernel) into <out-dir> for the node-assets image build. Mirrors the Packer
# provisioners deploy/packer/provisioners/install-{firecracker,fc-kernel}.sh
# so the K8s host fleet runs the SAME firecracker + kernel as the MIG.
#
# firecracker is a public release; the engram guest kernel is a private GH
# release asset (ADR 0025), so this needs `gh` auth, GH_TOKEN, or a
# pre-staged ENGRAM_KERNEL_SRC.
#
#   docker/node-assets-fetch.sh ./node-assets-ctx
#   docker build -f docker/node-assets.Dockerfile ./node-assets-ctx
set -euo pipefail

OUT="${1:?usage: node-assets-fetch.sh <out-dir>}"
mkdir -p "$OUT"
ARCH="$(uname -m)"

FC_VER="${FC_VER:-v1.10.1}"
echo "==> firecracker ${FC_VER} (${ARCH})"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fSL --retry 3 --retry-delay 2 "https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VER}/firecracker-${FC_VER}-${ARCH}.tgz" -o "$tmp/fc.tgz"
tar -xzf "$tmp/fc.tgz" -C "$tmp"
fc_bin="$(find "$tmp" -type f -name "firecracker-${FC_VER}-${ARCH}" | head -1)"
[ -n "$fc_bin" ] || { echo "could not find firecracker binary in the tarball" >&2; exit 1; }
install -m0755 "$fc_bin" "$OUT/firecracker"

REPO="${ENGRAM_KERNEL_REPO:-cortexapps/engrams}"
TAG="${ENGRAM_KERNEL_TAG:-fc-kernel-6.1.102-1}"
ASSET="${ENGRAM_KERNEL_ASSET:-vmlinux-engram-6.1.102-1}"
echo "==> engram guest kernel ${TAG}/${ASSET} from ${REPO}"
if [ -n "${ENGRAM_KERNEL_SRC:-}" ]; then
  install -m0644 "$ENGRAM_KERNEL_SRC" "$OUT/vmlinux"
elif command -v gh >/dev/null 2>&1; then
  # The GitHub asset API 500s intermittently on large assets — retry.
  n=0
  until gh release download "$TAG" --repo "$REPO" --pattern "$ASSET" --output "$OUT/vmlinux" --clobber; do
    n=$((n + 1))
    [ "$n" -ge 4 ] && { echo "kernel download failed after $n attempts" >&2; exit 1; }
    echo "   gh download failed (attempt $n); retrying in 5s..." >&2
    sleep 5
  done
  chmod 0644 "$OUT/vmlinux"
else
  : "${GH_TOKEN:?need gh CLI, GH_TOKEN, or ENGRAM_KERNEL_SRC (private repo asset)}"
  aid="$(curl -fsSL -H "Authorization: Bearer $GH_TOKEN" -H "Accept: application/vnd.github+json" \
      "https://api.github.com/repos/${REPO}/releases/tags/${TAG}" \
    | python3 -c "import sys,json;print(next((x['id'] for x in json.load(sys.stdin)['assets'] if x['name']=='${ASSET}'),''))")"
  [ -n "$aid" ] || { echo "asset ${ASSET} not found on release ${TAG}" >&2; exit 1; }
  # Auth header is for api.github.com; curl drops it on the redirect to the
  # signed asset URL, which is what the storage backend wants.
  curl -fSL --retry 3 --retry-delay 2 -H "Authorization: Bearer ${GH_TOKEN}" -H "Accept: application/octet-stream" \
    "https://api.github.com/repos/${REPO}/releases/assets/${aid}" -o "$OUT/vmlinux"
  chmod 0644 "$OUT/vmlinux"
fi

file "$OUT/vmlinux" | grep -q "ELF 64-bit" || { echo "kernel is not an ELF binary:" >&2; file "$OUT/vmlinux" >&2; exit 1; }
echo "==> staged into ${OUT}:"
ls -la "$OUT/firecracker" "$OUT/vmlinux"
