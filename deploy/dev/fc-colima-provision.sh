#!/usr/bin/env bash
# `just fc-colima-provision [profile]` — provision a dedicated Colima VM as
# an aarch64 Firecracker dev host (ADR 0068). Idempotent: safe to re-run
# against an already-provisioned (or partially-provisioned) profile. Only
# the host-agent + its FC stack run in this VM; the coordinator,
# orchestrator, web, and docker-compose deps stay on the Mac (`just dev-fc`
# wires that up — this script only does the VM side).
#
# Contract paths this script guarantees inside the VM (the Tiltfile /
# justfile `dev-fc` path depends on these exact paths):
#   /usr/local/bin/firecracker        - upstream FC binary, version pinned below
#   /opt/engram-dev/Image             - the aarch64 guest kernel (ADR 0025 recipe)
#   /opt/engram-dev/{bin,shared,var}  - working dirs for the host-agent
#
# Usage: deploy/dev/fc-colima-provision.sh [profile] [--rebuild-kernel]
#   profile           Colima profile name (default: fc-dev)
#   --rebuild-kernel  force a guest-kernel rebuild even if
#                     /opt/engram-dev/Image already exists
#
# Requires macOS on Apple Silicon + nested virtualization (Apple M3 or
# later, macOS 15+) to get a real /dev/kvm inside the VM — see ADR 0068.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

PROFILE="fc-dev"
REBUILD_KERNEL=0
for arg in "$@"; do
    case "$arg" in
        --rebuild-kernel) REBUILD_KERNEL=1 ;;
        -*)
            echo "fc-colima-provision: unknown flag '$arg'" >&2
            exit 1
            ;;
        *) PROFILE="$arg" ;;
    esac
done

# --- preconditions ---
if [ "$(uname -s)" != "Darwin" ] || [ "$(uname -m)" != "arm64" ]; then
    echo "fc-colima-provision: requires macOS on Apple Silicon (Darwin/arm64)" >&2
    exit 1
fi
for bin in colima jq; do
    command -v "$bin" >/dev/null 2>&1 || {
        echo "fc-colima-provision: '$bin' not found — install with 'brew install $bin'" >&2
        exit 1
    }
done
echo "NOTE: nested virtualization (a real /dev/kvm inside the VM) needs an" >&2
echo "      Apple M3-or-later chip on macOS 15+. Older hardware/OS boots the" >&2
echo "      VM fine but KVM won't be available — this script fails loudly at" >&2
echo "      the /dev/kvm check below rather than silently degrading to no" >&2
echo "      isolation." >&2

# Firecracker version: read CI's pin so the aarch64 dev rig and the x86_64
# CI lane run the same FC release train (ADR 0068). Falls back to a
# hardcoded value if ci.yml's format ever changes underneath this grep —
# keep that fallback in sync with the "Install firecracker" step there.
FC_VERSION="$(grep -oE 'FC_VER: v[0-9.]+' .github/workflows/ci.yml | head -1 | awk '{print $2}')"
FC_VERSION="${FC_VERSION:-v1.16.0}"

# --- docker context safety: `colima start` silently steals the docker CLI
# context, which would repoint every other `docker`/`docker compose`
# invocation on the Mac (including plain `just dev`'s compose stack) at
# this VM. Capture it up front and restore after every `colima start`,
# plus a final trap so a mid-script failure can't leave it swapped. ---
ORIG_CONTEXT="$(docker context show)"
restore_context() {
    local current
    current="$(docker context show 2>/dev/null || echo "$ORIG_CONTEXT")"
    if [ "$current" != "$ORIG_CONTEXT" ]; then
        echo "==> restoring docker context: '$current' -> '$ORIG_CONTEXT'"
        docker context use "$ORIG_CONTEXT" >/dev/null
    fi
}
trap restore_context EXIT

start_profile() {
    "$@"
    restore_context
}

# --- create/start the profile ---
profile_status="$(colima list --json 2>/dev/null | jq -rs --arg p "$PROFILE" 'map(select(.name==$p)) | .[0].status // empty')"
if [ -z "$profile_status" ]; then
    echo "==> profile '$PROFILE' does not exist; creating (vz + nested-virtualization)"
    start_profile colima start --profile "$PROFILE" --vm-type vz --nested-virtualization --cpu 6 --memory 8 --disk 60
elif [ "$profile_status" != "Running" ]; then
    echo "==> profile '$PROFILE' exists but is stopped ($profile_status); starting"
    start_profile colima start --profile "$PROFILE"
else
    echo "==> profile '$PROFILE' already running"
fi

ssh_() { colima ssh --profile "$PROFILE" -- "$@"; }

# --- verify /dev/kvm made it into the VM ---
if ! ssh_ ls /dev/kvm >/dev/null 2>&1; then
    cat >&2 <<EOF
fc-colima-provision: /dev/kvm not present in profile '$PROFILE'.
Nested virtualization needs an Apple M3-or-later chip running macOS 15+.
Check the 'colima start --nested-virtualization' output above for a warning,
and 'sysctl kern.hv_support' on the Mac host (should be 1).
EOF
    exit 1
fi
echo "==> /dev/kvm present in '$PROFILE'"

VM_USER="$(ssh_ id -un)"

# --- in-VM host prep (packages, firecracker binary, nbd/uffd/kvm-perms,
# loopback forwarders, working dirs). One ssh round-trip; every step below
# checks state before acting so re-runs are cheap and side-effect free. ---
echo "==> host prep + packages inside '$PROFILE'"
colima ssh --profile "$PROFILE" -- sudo bash -s -- "$FC_VERSION" "$VM_USER" <<'REMOTE'
set -euo pipefail
FC_VERSION="$1"
VM_USER="$2"

# --- apt packages ---
PKGS="build-essential flex bison bc libssl-dev libelf-dev dwarves curl git file socat iptables squashfs-tools e2fsprogs"
MISSING=""
for p in $PKGS; do
    dpkg -s "$p" >/dev/null 2>&1 || MISSING="$MISSING $p"
done
if [ -n "$MISSING" ]; then
    echo "==> apt-get install:$MISSING"
    apt-get update -qq
    # shellcheck disable=SC2086
    apt-get install -y -qq $MISSING
else
    echo "==> all apt packages already present"
fi

# --- firecracker binary (upstream aarch64 release, CI's pinned version) ---
NEED_FC=1
if [ -x /usr/local/bin/firecracker ]; then
    CURRENT="$(/usr/local/bin/firecracker --version 2>/dev/null | head -1 | grep -oE 'v[0-9.]+' || true)"
    [ "$CURRENT" = "$FC_VERSION" ] && NEED_FC=0
fi
if [ "$NEED_FC" = "1" ]; then
    echo "==> installing firecracker $FC_VERSION"
    TMP="$(mktemp -d)"
    curl -fSL "https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}/firecracker-${FC_VERSION}-aarch64.tgz" -o "$TMP/fc.tgz"
    tar -xzf "$TMP/fc.tgz" -C "$TMP"
    install -m 0755 "$TMP/release-${FC_VERSION}-aarch64/firecracker-${FC_VERSION}-aarch64" /usr/local/bin/firecracker
    rm -rf "$TMP"
else
    echo "==> firecracker $FC_VERSION already installed"
fi
/usr/local/bin/firecracker --version

# --- nbd: chunked-disk backing needs the module loaded with enough
# devices, persistently (ADR 0024 gotcha: base-snapshot capture hangs
# ~500s without it) ---
echo nbd >/etc/modules-load.d/engram-nbd.conf
echo "options nbd nbds_max=16" >/etc/modprobe.d/engram-nbd.conf
if lsmod | grep -q '^nbd '; then
    modprobe -r nbd 2>/dev/null || true
fi
modprobe nbd nbds_max=16

# --- uffd: unprivileged userfaultfd for FC snapshot page-faulting ---
echo "vm.unprivileged_userfaultfd=1" >/etc/sysctl.d/99-engram.conf
sysctl -w vm.unprivileged_userfaultfd=1 >/dev/null

# --- /dev/kvm perms, persisted across VM restarts via a udev rule (the
# device node is recreated on every boot, so a one-off chmod doesn't stick) ---
chmod 0666 /dev/kvm
cat >/etc/udev/rules.d/99-engram-kvm.rules <<'RULES'
KERNEL=="kvm", MODE="0666"
RULES
udevadm control --reload-rules 2>/dev/null || true
udevadm trigger --name-match=kvm 2>/dev/null || true

# --- loopback forwarders: preserve the Mac dev stack's loopback literalism
# (localhost:5001 image refs, STORAGE_EMULATOR_HOST=http://localhost:4443,
# OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317) unmodified inside the VM
# by forwarding to the Lima host gateway (ADR 0068 "Networking" — VM -> Mac
# direction). The host-agent env uses these localhost:PORT values as-is. ---
install_fwd_unit() {
    name="$1"
    listen_port="$2"
    target_port="$3"
    cat >"/etc/systemd/system/engram-fwd-${name}.service" <<UNIT
[Unit]
Description=engram dev: forward localhost:${listen_port} -> 192.168.5.2:${target_port}
After=network.target

[Service]
ExecStart=/usr/bin/socat TCP-LISTEN:${listen_port},fork,reuseaddr,bind=127.0.0.1 TCP:192.168.5.2:${target_port}
Restart=always

[Install]
WantedBy=multi-user.target
UNIT
}
install_fwd_unit registry 5001 5001   # OCI registry (image pulls)
install_fwd_unit gcs 4443 4443        # fake-gcs-server (chunk store)
install_fwd_unit jaeger 4317 4317     # OTLP/gRPC (ADR 0019 tracing)
systemctl daemon-reload
systemctl enable --now engram-fwd-registry.service engram-fwd-gcs.service engram-fwd-jaeger.service

# --- working dirs for the host-agent ---
mkdir -p /opt/engram-dev/bin /opt/engram-dev/shared /opt/engram-dev/var
chown -R "${VM_USER}:${VM_USER}" /opt/engram-dev

echo "==> host prep complete"
REMOTE

# --- guest kernel: build the aarch64 Image inside the VM (ADR 0025 recipe,
# Task-1 ARCH support). Never build over the virtiofs-mounted repo path —
# known-bad — so stage deploy/kernel/ into a VM-local directory first. ---
echo "==> guest kernel"
kernel_exists="$(ssh_ bash -c '[ -e /opt/engram-dev/Image ] && echo yes || echo no')"
if [ "$REBUILD_KERNEL" = "1" ] && [ "$kernel_exists" = "yes" ]; then
    echo "    --rebuild-kernel given; removing existing /opt/engram-dev/Image"
    ssh_ sudo rm -f /opt/engram-dev/Image
    kernel_exists="no"
fi
if [ "$kernel_exists" = "yes" ]; then
    echo "    /opt/engram-dev/Image already exists; skipping build."
    echo "    Force a rebuild with: $(basename "$0") $PROFILE --rebuild-kernel"
    echo "    (or: colima ssh --profile $PROFILE -- sudo rm /opt/engram-dev/Image)"
else
    echo "    staging deploy/kernel/ into the VM (VM-local dir, not virtiofs)"
    tar -C deploy/kernel -cf - build-fc-kernel.sh engram-docker.fragment microvm-kernel-ci-aarch64-6.1.config \
        | ssh_ bash -c 'rm -rf /opt/engram-dev/kernel-build && mkdir -p /opt/engram-dev/kernel-build && tar -C /opt/engram-dev/kernel-build -xf -'
    echo "    building the aarch64 guest kernel natively in-VM (~4min on 6 vcpus)"
    ssh_ bash -c '
        set -euo pipefail
        cd /opt/engram-dev/kernel-build
        mkdir -p out work
        ARCH=arm64 OUT="$(pwd)/out" WORK="$(pwd)/work" JOBS="$(nproc)" bash ./build-fc-kernel.sh
        asset="$(find out -maxdepth 1 -name "Image-engram-*" ! -name "*.sha256" | head -1)"
        [ -n "$asset" ] || { echo "kernel build: no Image-engram-* artifact produced" >&2; exit 1; }
        install -m 0644 "$asset" /opt/engram-dev/Image
    '
    echo "    installed /opt/engram-dev/Image"
fi

# --- summary ---
cat <<EOF

==> fc-colima-provision done for profile '$PROFILE'

Provisioned inside the VM:
  - packages: build-essential flex bison bc libssl-dev libelf-dev dwarves curl git file socat iptables squashfs-tools e2fsprogs
  - /usr/local/bin/firecracker ($FC_VERSION)
  - nbd loaded (nbds_max=16), vm.unprivileged_userfaultfd=1, /dev/kvm mode 0666 (persisted)
  - engram-fwd-registry.service (localhost:5001 -> 192.168.5.2:5001)
  - engram-fwd-gcs.service      (localhost:4443 -> 192.168.5.2:4443)
  - engram-fwd-jaeger.service   (localhost:4317 -> 192.168.5.2:4317)
  - /opt/engram-dev/{bin,shared,var}

Contract paths:
  /usr/local/bin/firecracker
  /opt/engram-dev/Image
  /opt/engram-dev/{bin,shared,var}

Next: just dev-fc $PROFILE
EOF
