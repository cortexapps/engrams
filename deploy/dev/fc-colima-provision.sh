#!/usr/bin/env bash
# `just fc-colima-provision [profile]` — provision a dedicated Colima VM as
# an aarch64 Firecracker dev host (ADR 0082). Idempotent: safe to re-run
# against an already-provisioned (or partially-provisioned) profile. Only
# the host-agent + its FC stack run in this VM; the coordinator,
# orchestrator, web, and docker-compose deps stay on the Mac (`just dev-fc`
# wires that up — this script only does the VM side).
#
# Contract paths this script guarantees inside the VM (the Tiltfile /
# justfile `dev-fc` path depends on these exact paths):
#   /usr/local/bin/firecracker        - upstream FC binary, version pinned below
#   /opt/engram-dev/Image             - the aarch64 guest kernel (ADR 0025 recipe)
#   /opt/engram-dev/bin/mke2fs        - e2fsprogs 1.47.2, libarchive tar input
#   /opt/engram-dev/bin/debugfs       - matching debugfs for the tar-input gate
#   /opt/engram-dev/{bin,shared,var}  - working dirs for the host-agent
#
# Usage: deploy/dev/fc-colima-provision.sh [profile] [--rebuild-kernel]
#   profile           Colima profile name (default: fc-dev)
#   --rebuild-kernel  force a guest-kernel rebuild even if
#                     /opt/engram-dev/Image already exists
#
# Requires macOS on Apple Silicon + nested virtualization (Apple M3 or
# later, macOS 15+) to get a real /dev/kvm inside the VM — see ADR 0082.
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
# CI lane run the same FC release train (ADR 0082). Falls back to a
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
    # 16 GiB: the host-agent keeps each enabled image's base-snapshot memory
    # image resident (~4 GiB for the default session budget), so an 8 GiB VM
    # can't fit that warm cache + a 4 GiB session + OS/host-agent overhead —
    # sessions queue forever with "no capacity". 16 GiB leaves room for the
    # cache + a couple of sessions.
    start_profile colima start --profile "$PROFILE" --vm-type vz --nested-virtualization --cpu 6 --memory 16 --disk 60
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
PKGS="build-essential flex bison bc libssl-dev libelf-dev dwarves curl git file iptables squashfs-tools e2fsprogs libarchive-dev pkg-config"
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

# --- mke2fs: enable-time image materialization runs in the VM-side
# host-agent, so give it a stable contract path independent of sudo's PATH.
# Ubuntu's packaged e2fsprogs is 1.47.0 in current Colima images and cannot
# pack tar inputs. Build the pinned, libarchive-enabled toolchain used by the
# rest of dev/prod and gate it with a tar metadata smoke.
E2FSPROGS_VERSION="1.47.2"
E2FSPROGS_URL="https://mirrors.edge.kernel.org/pub/linux/kernel/tools/e2fsprogs/v${E2FSPROGS_VERSION}/e2fsprogs-${E2FSPROGS_VERSION}.tar.gz"
E2FSPROGS_PREFIX="/opt/engram-dev/e2fsprogs"
MKE2FS_CONTRACT="/opt/engram-dev/bin/mke2fs"
DEBUGFS_CONTRACT="/opt/engram-dev/bin/debugfs"

mke2fs_version() {
    "$1" -V 2>&1 | sed -n 's/^mke2fs \([0-9][0-9.]*\).*/\1/p' | head -1 || true
}

tar_input_smoke() {
    local mke2fs_bin="$1"
    local debugfs_bin="$2"
    local scratch stat_out
    scratch="$(mktemp -d)"
    mkdir -p "$scratch/root/bin"
    printf 'setuid smoke\n' >"$scratch/root/bin/setuid-probe"
    tar --numeric-owner --owner=0 --group=0 --mode=0755 --no-recursion \
        -cf "$scratch/input.tar" -C "$scratch/root" . ./bin || {
        rm -rf "$scratch"
        return 1
    }
    tar --numeric-owner --owner=0 --group=0 --mode=04755 \
        -rf "$scratch/input.tar" -C "$scratch/root" ./bin/setuid-probe || {
        rm -rf "$scratch"
        return 1
    }
    "$mke2fs_bin" -q -F -t ext4 -d "$scratch/input.tar" "$scratch/rootfs.ext4" 4m || {
        rm -rf "$scratch"
        return 1
    }
    stat_out="$("$debugfs_bin" -R "stat /bin/setuid-probe" "$scratch/rootfs.ext4" 2>/dev/null || true)"
    printf '%s\n' "$stat_out"
    printf '%s\n' "$stat_out" | grep -Eq 'User:[[:space:]]+0[[:space:]]+Group:[[:space:]]+0' || {
        echo "mke2fs tar input failed to preserve uid/gid 0 from the tar header (ADR 0084)" >&2
        rm -rf "$scratch"
        return 1
    }
    printf '%s\n' "$stat_out" | grep -Eq 'Mode:[[:space:]]+04755' || {
        echo "mke2fs tar input failed to preserve mode 04755 from the tar header (ADR 0084)" >&2
        rm -rf "$scratch"
        return 1
    }
    rm -rf "$scratch"
}

install_e2fsprogs() {
    local tmp jobs
    tmp="$(mktemp -d)"
    jobs="$(nproc 2>/dev/null || echo 2)"
    echo "==> building e2fsprogs ${E2FSPROGS_VERSION} with libarchive"
    curl -fSL "$E2FSPROGS_URL" -o "$tmp/e2fsprogs.tar.gz"
    tar -xzf "$tmp/e2fsprogs.tar.gz" -C "$tmp"
    (
        cd "$tmp/e2fsprogs-${E2FSPROGS_VERSION}"
        LDFLAGS="-Wl,-rpath,${E2FSPROGS_PREFIX}/lib" \
            ./configure --prefix="$E2FSPROGS_PREFIX" --with-libarchive=direct
        make -j"$jobs"
        make install
    )
    install -d -m 0755 /opt/engram-dev/bin
    ln -sf "$E2FSPROGS_PREFIX/sbin/mke2fs" "$MKE2FS_CONTRACT"
    ln -sf "$E2FSPROGS_PREFIX/sbin/debugfs" "$DEBUGFS_CONTRACT"
    if find "$E2FSPROGS_PREFIX/lib" -maxdepth 1 -name '*.so*' -print -quit 2>/dev/null | grep -q .; then
        echo "$E2FSPROGS_PREFIX/lib" >/etc/ld.so.conf.d/engram-e2fsprogs.conf
        ldconfig
    fi
    rm -rf "$tmp"
}

install -d -m 0755 /opt/engram-dev/bin
if [ -x "$MKE2FS_CONTRACT" ] && [ -x "$DEBUGFS_CONTRACT" ] &&
   [ "$(mke2fs_version "$MKE2FS_CONTRACT")" = "$E2FSPROGS_VERSION" ] &&
   tar_input_smoke "$MKE2FS_CONTRACT" "$DEBUGFS_CONTRACT" >/dev/null; then
    echo "==> mke2fs ${E2FSPROGS_VERSION} tar-input gate already passes"
else
    install_e2fsprogs
fi

MKE2FS_VER="$(mke2fs_version "$MKE2FS_CONTRACT")"
if [ "$MKE2FS_VER" != "$E2FSPROGS_VERSION" ]; then
    echo "fc-colima-provision: mke2fs $MKE2FS_VER != $E2FSPROGS_VERSION at $MKE2FS_CONTRACT" >&2
    exit 1
fi
echo "==> mke2fs: $MKE2FS_CONTRACT -> $(readlink "$MKE2FS_CONTRACT")"
"$MKE2FS_CONTRACT" -V
echo "==> mke2fs tar-input smoke"
tar_input_smoke "$MKE2FS_CONTRACT" "$DEBUGFS_CONTRACT" >/dev/null

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
# OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317) unmodified inside the VM by
# routing them to the Lima host gateway (ADR 0082 "Networking" — VM -> Mac). The
# host-agent env uses these localhost:PORT values as-is.
#
# We DNAT the loopback destination to the gateway rather than run a socat
# LISTENER on 127.0.0.1. Lima's default portForwards forward EVERY guest
# 127.0.0.1 listener back to the Mac's 127.0.0.1 (the `guestIP: 127.0.0.1` rule
# colima generates), and colima 0.9.x exposes no knob to suppress it. So a socat
# listener on 127.0.0.1:5001 gets surfaced onto the Mac's :5001 and SHADOWS the
# real registry there — 127.0.0.1 beats the deps' 0.0.0.0 forward, so every
# Mac-side `localhost:5001` (bake push, coordinator dial) hits the VM loop and
# fails. An OUTPUT DNAT has no listener for Lima to forward, so nothing is
# shadowed, while the guest's own `localhost:PORT` still reaches the Mac dep via
# the gateway — and engram-oci's loopback-only-plaintext allowance still applies
# because the ref STRING is unchanged. MASQUERADE rewrites the loopback source so
# the gateway's replies route back. route_localnet lets the kernel DNAT a
# loopback-destined packet out to a remote (same trick the egress proxy uses).
cat >/usr/local/bin/engram-dev-fwd.sh <<'FWD'
#!/usr/bin/env bash
# Route the VM's localhost:{5001,4443,4317} to the Mac dev stack via the Lima
# gateway WITHOUT a loopback listener (Lima would forward a listener back to the
# Mac and shadow the real deps). See fc-colima-provision.sh for the rationale.
set -euo pipefail
sysctl -w net.ipv4.conf.all.route_localnet=1 >/dev/null
sysctl -w net.ipv4.conf.lo.route_localnet=1 >/dev/null
gw=192.168.5.2
for port in 5001 4443 4317; do
    dnat="-p tcp -d 127.0.0.1 --dport $port -m comment --comment engram-dev-fwd -j DNAT --to-destination $gw:$port"
    masq="-p tcp -d $gw --dport $port -m comment --comment engram-dev-fwd -j MASQUERADE"
    iptables -t nat -C OUTPUT $dnat 2>/dev/null || iptables -t nat -A OUTPUT $dnat
    iptables -t nat -C POSTROUTING $masq 2>/dev/null || iptables -t nat -A POSTROUTING $masq
done
FWD
chmod +x /usr/local/bin/engram-dev-fwd.sh
cat >/etc/systemd/system/engram-dev-fwd.service <<'UNIT'
[Unit]
Description=engram dev: route localhost:{5001,4443,4317} to the Mac dev stack (DNAT; no shadowing listener)
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/bin/engram-dev-fwd.sh

[Install]
WantedBy=multi-user.target
UNIT
# Retire the old socat forwarders if a prior provision installed them — their
# 127.0.0.1 listeners are exactly what Lima surfaced onto the Mac and shadowed
# the real deps with.
for old in registry gcs jaeger; do
    if [ -e "/etc/systemd/system/engram-fwd-${old}.service" ]; then
        systemctl disable --now "engram-fwd-${old}.service" 2>/dev/null || true
        rm -f "/etc/systemd/system/engram-fwd-${old}.service"
    fi
done
systemctl daemon-reload
systemctl enable --now engram-dev-fwd.service

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
    if ssh_ bash -c '[ -d /opt/engram-dev/kernel-build ]'; then
        echo "    removing stale /opt/engram-dev/kernel-build work tree"
        ssh_ sudo rm -rf /opt/engram-dev/kernel-build
    fi
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
    echo "    removing /opt/engram-dev/kernel-build work tree"
    ssh_ sudo rm -rf /opt/engram-dev/kernel-build
fi

# --- summary ---
cat <<EOF

==> fc-colima-provision done for profile '$PROFILE'

Provisioned inside the VM:
  - packages: build-essential flex bison bc libssl-dev libelf-dev dwarves curl git file iptables squashfs-tools e2fsprogs
  - /opt/engram-dev/bin/mke2fs (stable contract path for enable-time materialize)
  - /usr/local/bin/firecracker ($FC_VERSION)
  - nbd loaded (nbds_max=16), vm.unprivileged_userfaultfd=1, /dev/kvm mode 0666 (persisted)
  - engram-dev-fwd.service (DNAT localhost:{5001,4443,4317} -> 192.168.5.2, no
    shadowing loopback listener — see the script comment for why not socat)
  - /opt/engram-dev/{bin,shared,var}

Contract paths:
  /usr/local/bin/firecracker
  /opt/engram-dev/Image
  /opt/engram-dev/{bin,shared,var}

Next: just dev-fc $PROFILE
EOF
