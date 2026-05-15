#!/usr/bin/env bash
# Install the Firecracker guest kernel at /usr/local/lib/engram/vmlinux.
# Pinned to vmlinux-5.10.223 from Firecracker's CI bucket — the same
# kernel the dev / integration-test path uses via
# crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh.
#
# The host-agent's TF default `kernel_image_path` points at this exact
# path (see deploy/terraform/gcp/modules/fc-host-mig/variables.tf).
# Without this provisioner, the bake produced an image where the
# host-agent rejected every sandbox spec with "kernel_image_path does
# not exist" — every session 503'd before booting.
set -euo pipefail

KERNEL_URL="${KERNEL_URL:-https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.10/x86_64/vmlinux-5.10.223}"
DEST="/usr/local/lib/engram/vmlinux"

sudo mkdir -p "$(dirname "$DEST")"

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT

echo "==> downloading FC guest kernel from $KERNEL_URL"
# --fail makes curl exit non-zero on HTTP 4xx/5xx so we don't end up
# with an HTML error page on disk masquerading as a kernel.
curl -fSL --silent --show-error "$KERNEL_URL" -o "$tmp"

sudo install -m 0644 "$tmp" "$DEST"
echo "==> kernel installed: $DEST ($(stat -c '%s' "$DEST") bytes)"
