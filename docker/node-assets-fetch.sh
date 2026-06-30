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

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Firecracker. ADR 0045 Phase B: a pre-staged binary (ENGRAM_FC_SRC — the
# forked build from the `build-firecracker` CI job, carrying the MAP_SHARED +
# Msync surface) wins; otherwise download the pinned upstream release. This
# mirrors the kernel's ENGRAM_KERNEL_SRC override, so the fork plugs in at the
# same seam without changing the node-assets image contract (still a single
# `$OUT/firecracker`). With ENGRAM_FC_SRC unset this is identical to before.
FC_VER="${FC_VER:-v1.16.0}"
if [ -n "${ENGRAM_FC_SRC:-}" ]; then
  echo "==> firecracker from ENGRAM_FC_SRC=${ENGRAM_FC_SRC} (forked build)"
  [ -f "$ENGRAM_FC_SRC" ] || { echo "ENGRAM_FC_SRC is set but not a file: $ENGRAM_FC_SRC" >&2; exit 1; }
  install -m0755 "$ENGRAM_FC_SRC" "$OUT/firecracker"
else
  echo "==> firecracker ${FC_VER} (${ARCH}) from upstream release"
  curl -fSL --retry 3 --retry-delay 2 "https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VER}/firecracker-${FC_VER}-${ARCH}.tgz" -o "$tmp/fc.tgz"
  tar -xzf "$tmp/fc.tgz" -C "$tmp"
  fc_bin="$(find "$tmp" -type f -name "firecracker-${FC_VER}-${ARCH}" | head -1)"
  [ -n "$fc_bin" ] || { echo "could not find firecracker binary in the tarball" >&2; exit 1; }
  install -m0755 "$fc_bin" "$OUT/firecracker"
fi

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

# ── RO session bundles (ADR 0027 → 0055 → 0058) ─────────────────────────────
# The sentinel + skills + playwright + integrations-cli squashfs bundles + the current.json stamp
# (logical name -> sha256). The host-agent reads these from
# /var/lib/engram/shared at startup; the engram-host-fleet init container copies
# them out of this image. ADR 0055: skills are profile-selected per session — the
# coord resolves a session's selected_skills names against this stamp and
# patch_drives each into a reserved dyn-* slot. Pull the published OCI artifacts
# (the publish-bundles job) at :main so a node-assets bake always carries the
# current fleet bundles. Needs `oras` + GHCR auth.
BUNDLE_REPO="${ENGRAM_BUNDLE_REPO:-ghcr.io/cortexapps/engrams}"
BUNDLE_TAG="${ENGRAM_BUNDLE_TAG:-main}"
BUNDLES_OUT="$OUT/bundles"
mkdir -p "$BUNDLES_OUT"

stage_bundle() {  # stage_bundle <name>; echoes the staged sha256 on stdout
  local name="$1"
  echo "==> bundle ${name} (${BUNDLE_REPO}/bundle-${name}:${BUNDLE_TAG})" >&2
  ( cd "$tmp" && rm -f "${name}.squashfs" \
    && oras pull "${BUNDLE_REPO}/bundle-${name}:${BUNDLE_TAG}" >&2 )
  [ -f "$tmp/${name}.squashfs" ] || { echo "oras pull yielded no ${name}.squashfs" >&2; return 1; }
  local sha
  sha="$(sha256sum "$tmp/${name}.squashfs" | awk '{print $1}')"
  # ADR 0055: content-keyed filename (<sha>.squashfs), matching
  # AuxRoDrive::staged_file_name — NOT a <name>- prefix, so a skill staged once
  # dedups across whatever reserved slot it lands in.
  install -m0644 "$tmp/${name}.squashfs" "$BUNDLES_OUT/${sha}.squashfs"
  echo "$sha"
}

# ADR 0055: `sentinel` is MANDATORY — base-snapshot capture resolves every
# reserved dyn-* slot to its sha, so a fleet without it can't capture any base.
sentinel_sha="$(stage_bundle sentinel)"
skills_sha="$(stage_bundle skills)"
playwright_sha="$(stage_bundle playwright)"
# ADR 0058: the integration CLI toolbox (gh + datadog-ci), profile-selected
# when a connector with a bundled `cli` facet is enabled.
integrations_cli_sha="$(stage_bundle integrations-cli)"
# ADR 0062: the built-in `claude` harness rides the stamp like a skill — the coord
# resolves the built-in's squashfs from this stamp (key `harness-claude`) + its
# embedded descriptor, mounts it on dyn_0, and the session execs it. No registration.
harness_claude_sha="$(stage_bundle harness-claude)"
# Stamp: logical name -> sha256, matching AuxRoDrive::CURRENT_STAMP / read_stamp().
printf '{"sentinel":"%s","skills":"%s","playwright":"%s","integrations-cli":"%s","harness-claude":"%s"}\n' \
  "$sentinel_sha" "$skills_sha" "$playwright_sha" "$integrations_cli_sha" "$harness_claude_sha" \
  > "$BUNDLES_OUT/current.json"

echo "==> staged into ${OUT}:"
ls -la "$OUT/firecracker" "$OUT/vmlinux" "$BUNDLES_OUT"
