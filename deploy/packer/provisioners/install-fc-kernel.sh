#!/usr/bin/env bash
# Install the Firecracker guest kernel at /usr/local/lib/engram/vmlinux.
#
# ADR 0025: this is now engram's *own* guest kernel (stock Firecracker
# microvm config + deploy/kernel/engram-docker.fragment — nf_tables + the
# `raw` table so Docker/compose run inside a sandbox), published as a
# GitHub release asset by .github/workflows/build-fc-kernel.yml. Keep the
# TAG/ASSET in sync with deploy/kernel/build-fc-kernel.sh.
#
# The host-agent's TF default `kernel_image_path` points at this exact path
# (deploy/terraform/gcp/modules/fc-host-mig/variables.tf) — install path is
# unchanged from the old FC-CI kernel, only the source moved.
#
# cortexapps/engrams is PRIVATE, so the asset is not anonymous. Resolution
# order (first that works wins):
#   1. ENGRAM_KERNEL_SRC — a pre-staged local file (the FC-host bake CI, which
#      already holds a token, can download it once and pass the path in).
#   2. `gh release download` (needs gh + GH_TOKEN/gh auth).
#   3. curl against the GitHub asset API with $GH_TOKEN.
set -euo pipefail

REPO="${ENGRAM_KERNEL_REPO:-cortexapps/engrams}"
TAG="${ENGRAM_KERNEL_TAG:-fc-kernel-6.1.102-1}"
ASSET="${ENGRAM_KERNEL_ASSET:-vmlinux-engram-6.1.102-1}"
DEST="/usr/local/lib/engram/vmlinux"

sudo mkdir -p "$(dirname "$DEST")"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

if [ -n "${ENGRAM_KERNEL_SRC:-}" ]; then
  echo "==> using pre-staged kernel $ENGRAM_KERNEL_SRC"
  cp "$ENGRAM_KERNEL_SRC" "$tmp"
elif command -v gh >/dev/null 2>&1; then
  echo "==> gh release download $TAG / $ASSET from $REPO"
  gh release download "$TAG" --repo "$REPO" --pattern "$ASSET" --output "$tmp" --clobber
else
  : "${GH_TOKEN:?need GH_TOKEN (private repo) or gh CLI or ENGRAM_KERNEL_SRC}"
  echo "==> resolving asset id for $TAG / $ASSET via API"
  asset_id="$(curl -fsSL -H "Authorization: Bearer $GH_TOKEN" \
      -H "Accept: application/vnd.github+json" \
      "https://api.github.com/repos/$REPO/releases/tags/$TAG" \
    | python3 -c "import sys,json;a=[x['id'] for x in json.load(sys.stdin)['assets'] if x['name']=='$ASSET'];print(a[0] if a else '')")"
  [ -n "$asset_id" ] || { echo "asset $ASSET not found on release $TAG" >&2; exit 1; }
  # Auth header is for api.github.com; curl drops it on the cross-host
  # redirect to the signed asset URL, which is what S3 wants.
  curl -fSL -H "Authorization: Bearer $GH_TOKEN" -H "Accept: application/octet-stream" \
    "https://api.github.com/repos/$REPO/releases/assets/$asset_id" -o "$tmp"
fi

# Guard against an HTML error page masquerading as a kernel.
file "$tmp" | grep -q "ELF 64-bit" || { echo "downloaded file is not an ELF kernel:" >&2; file "$tmp" >&2; exit 1; }

sudo install -m 0644 "$tmp" "$DEST"
echo "==> kernel installed: $DEST ($(stat -c '%s' "$DEST") bytes)"
