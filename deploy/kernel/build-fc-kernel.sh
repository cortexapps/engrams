#!/usr/bin/env bash
# Build the engram Firecracker guest kernel (ADR 0025).
#
# = Firecracker's stock microvm guest config (FC-bootable: virtio-mmio +
#   vsock + ip_pnp) + deploy/kernel/engram-docker.fragment (the netfilter
#   stack Docker needs in-guest). The stock FC-CI kernels — every published
#   5.10 and 6.1 binary — strip nf_tables and the `raw` table, which blocks
#   dockerd/compose inside a sandbox; this rebuild restores them.
#
# Output: $OUT/vmlinux-engram-${LINUX_VERSION}-${KERNEL_REV}
# Published as a GitHub release asset by .github/workflows/build-fc-kernel.yml;
# consumed by deploy/packer/provisioners/install-fc-kernel.sh (prod host bake)
# and crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh (dev).
#
# Reproducible: pinned linux source version + pinned Firecracker base-config
# ref + the in-repo fragment. Bump KERNEL_REV when the fragment changes.
#
# Build deps (Debian/Ubuntu): build-essential bc bison flex libelf-dev libssl-dev cpio
set -euo pipefail

# --- pins (keep RELEASE_TAG / ASSET in sync with the consumers) ---
LINUX_VERSION="${LINUX_VERSION:-6.1.102}"
KERNEL_REV="${KERNEL_REV:-1}"                    # engram build revision; bump on fragment/base change

ASSET="vmlinux-engram-${LINUX_VERSION}-${KERNEL_REV}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FRAGMENT="${SCRIPT_DIR}/engram-docker.fragment"
# Vendored base config = Firecracker's microvm-kernel-ci-x86_64-6.1.config at
# firecracker SHA 8a7e8a01 (their "full" microvm config, PCI/FUSE on). We vendor
# it rather than download a release tag because the choice is boot-critical:
# Firecracker's stripped *release* config (e.g. v1.10.1) drops FUSE and other
# symbols and the guest boots but agentd never comes up — dev-vm-verified. Re-sync
# from upstream deliberately (and bump KERNEL_REV) when pulling driver/security updates.
BASE_CONFIG="${SCRIPT_DIR}/microvm-kernel-ci-x86_64-6.1.config"
WORK="${WORK:-$(pwd)/kbuild}"
OUT="${OUT:-$(pwd)}"
JOBS="${JOBS:-$(nproc)}"

mkdir -p "$WORK" && cd "$WORK"

src="linux-${LINUX_VERSION}"
if [ ! -d "$src" ]; then
  echo "==> downloading linux-${LINUX_VERSION} source"
  curl -fSL "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${LINUX_VERSION}.tar.xz" -o linux.tar.xz
  tar -xf linux.tar.xz
fi
cd "$src"

echo "==> base config: vendored ${BASE_CONFIG}"
cp "$BASE_CONFIG" .config

echo "==> merging ${FRAGMENT}"
./scripts/kconfig/merge_config.sh -m .config "$FRAGMENT"
make olddefconfig

# Fail loudly if the merge didn't take (Docker needs) or an FC-boot option
# regressed — a silently-wrong kernel would brick every session.
echo "==> asserting required symbols"
require() {
  grep -q "^$1=y" .config || { echo "MISSING required config: $1" >&2; exit 1; }
}
# FUSE: stripped by FC's release config -> agentd never boots (dev-vm-verified).
# NAMESPACES/USER_NS/PID_NS/NET_NS/SECCOMP*: required for chromium's
# unprivileged namespace (zygote) sandbox (ADR 0065 §7, issue #569) — a future
# base-config re-sync must not silently drop them.
for c in \
  CONFIG_NF_TABLES CONFIG_NFT_COMPAT CONFIG_NFT_NAT \
  CONFIG_IP_NF_RAW CONFIG_IP6_NF_NAT CONFIG_IP6_NF_RAW \
  CONFIG_BRIDGE_NETFILTER CONFIG_VXLAN CONFIG_OVERLAY_FS CONFIG_BRIDGE CONFIG_VETH \
  CONFIG_VIRTIO_MMIO CONFIG_VIRTIO_BLK CONFIG_VIRTIO_NET CONFIG_VIRTIO_VSOCKETS \
  CONFIG_IP_PNP CONFIG_EXT4_FS \
  CONFIG_FUSE_FS \
  CONFIG_NAMESPACES CONFIG_USER_NS CONFIG_PID_NS CONFIG_NET_NS \
  CONFIG_SECCOMP CONFIG_SECCOMP_FILTER ; do
  require "$c"
done
echo "    all required symbols present"

echo "==> building vmlinux (-j${JOBS})"
make -j"${JOBS}" vmlinux

cp vmlinux "${OUT}/${ASSET}"
echo "==> built ${OUT}/${ASSET}"
ls -lh "${OUT}/${ASSET}"
( cd "$OUT" && sha256sum "${ASSET}" | tee "${ASSET}.sha256" )
