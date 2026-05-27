#!/usr/bin/env bash
# Install engram-uffd-handler at /usr/local/bin/engram-uffd-handler.
# Pulled from a GCS URL the operator's CI populated before this Packer
# build kicks off — same convention as install-host-agent.sh. Static-musl
# binary so the image doesn't need a Rust toolchain.
#
# ADR 0020 Route B: the handler is co-located with `firecracker` on the FC
# host. On a UFFD restore (ENGRAM_FC_RESTORE_MODE=uffd) the host-agent spawns
# it, hands it the guest's userfaultfd via SCM_RIGHTS over a same-host UDS,
# and it serves every guest page fault from the chunk cache.

set -euo pipefail

: "${UFFD_HANDLER_GCS_URL:?UFFD_HANDLER_GCS_URL must be a gs:// URL pointing at the pre-built binary}"

echo "==> downloading engram-uffd-handler from $UFFD_HANDLER_GCS_URL"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

gsutil cp "$UFFD_HANDLER_GCS_URL" "$tmp/engram-uffd-handler"
file "$tmp/engram-uffd-handler"

# Static-musl ELF; chmod and install.
sudo install -m 0755 "$tmp/engram-uffd-handler" /usr/local/bin/engram-uffd-handler
/usr/local/bin/engram-uffd-handler --help >/dev/null 2>&1 || true

echo "==> engram-uffd-handler installed"

# Firecracker creates the guest's userfaultfd and SCM_RIGHTS-passes the fd to
# the handler. When FC runs jailed (dropped to a non-root uid without
# CAP_SYS_PTRACE) the kernel gates the userfaultfd(2) syscall behind
# `vm.unprivileged_userfaultfd`. Enable it persistently so UFFD restore works
# under the jailer. This is inherent to the chunked-memory UFFD design (ADR
# 0007/0020); review before baking if your host's threat model restricts
# unprivileged userfaultfd.
echo "==> enabling vm.unprivileged_userfaultfd=1 (FC UFFD restore under jailer)"
echo 'vm.unprivileged_userfaultfd = 1' | sudo tee /etc/sysctl.d/60-engram-uffd.conf >/dev/null
sudo sysctl -p /etc/sysctl.d/60-engram-uffd.conf
