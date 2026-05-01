#!/usr/bin/env bash
# Fetch the arm64 Linux kernel that VZ uses to boot Linux guests.
#
# Source: Kata Containers static-kernel release (Linux 6.18.15-186
# with a VZ-tuned kconfig). Same kernel `apple/container` uses by
# default — strips PCI/ACPI/USB/sound/graphics, ships
# VIRTIO_BLK/NET/CONSOLE built in. Cold boot is sub-second on
# Apple Silicon.
#
# License: Linux is GPLv2; the Kata release is GPLv2.

set -euo pipefail

CACHE_DIR="${HOME}/.cache/engram-vz-test"
DST="${CACHE_DIR}/vmlinux-arm64"
KATA_VERSION="3.17.0"
KATA_URL="https://github.com/kata-containers/kata-containers/releases/download/${KATA_VERSION}/kata-static-${KATA_VERSION}-arm64.tar.xz"
# Linux 6.12.28 with Kata's VZ-tuned kconfig. The tarball has
# multiple kernels (dragonball-experimental, nvidia-gpu, etc.); we
# want the plain virtio one. `vmlinux.container` is a symlink to
# this exact filename, so refer to it directly to avoid a separate
# symlink-resolution step.
KERNEL_INSIDE="./opt/kata/share/kata-containers/vmlinux-6.12.28-153"

mkdir -p "${CACHE_DIR}"

if [ -f "${DST}" ]; then
    echo "kernel already cached at ${DST}"
    ls -lh "${DST}"
    exit 0
fi

echo "downloading Kata ${KATA_VERSION} arm64 static kernel (~290 MB tarball, ~25 MB extracted)..."

TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT

curl -fSL -o "${TMP}/kata.tar.xz" "${KATA_URL}"

# Extract just the kernel.
( cd "${TMP}" && \
  tar -xJf kata.tar.xz \
      "${KERNEL_INSIDE}" )

mv "${TMP}/${KERNEL_INSIDE#./}" "${DST}"

file "${DST}"
ls -lh "${DST}"
