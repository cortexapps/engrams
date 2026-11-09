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
#   /usr/local/bin/firecracker        - the forked FC (third_party/firecracker),
#                                       built in the VM for aarch64
#   /opt/engram-dev/Image             - the aarch64 guest kernel (ADR 0025 recipe)
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

# --- in-VM host prep (packages, uffd/kvm-perms, loopback forwarders,
# working dirs). One ssh round-trip; every step below checks state before
# acting so re-runs are cheap and side-effect free. ---
echo "==> host prep + packages inside '$PROFILE'"
colima ssh --profile "$PROFILE" -- sudo bash -s -- "$VM_USER" <<'REMOTE'
set -euo pipefail
VM_USER="$1"

# --- apt packages: kernel build deps, Firecracker build deps (cmake, clang,
# musl-tools, libseccomp-dev, linux-libc-dev), and host tools ---
PKGS="build-essential flex bison bc libssl-dev libelf-dev dwarves cmake clang musl-tools libseccomp-dev linux-libc-dev curl git file iptables squashfs-tools thin-provisioning-tools"
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

# --- kernel uapi headers for the musl Firecracker build: expose only the
# libc-agnostic dirs inside musl's include root. A global -I/usr/include
# leaks glibc headers into the aws-lc-sys musl compile and breaks it. ---
MUSL_INC=/usr/include/aarch64-linux-musl
ln -sfn /usr/include/linux                 "$MUSL_INC/linux"
ln -sfn /usr/include/asm-generic           "$MUSL_INC/asm-generic"
ln -sfn /usr/include/aarch64-linux-gnu/asm "$MUSL_INC/asm"

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

# --- firecracker: the fork in third_party/firecracker, which is the binary
# production ships. Stock Firecracker rejects the fork's snapshot API (for
# example `defer_sync` on /snapshot/create), so captures fail on it. The
# submodule gitlink SHA is the version: /opt/engram-dev/firecracker.sha holds
# the SHA of the installed binary, and a re-run builds again only when the
# gitlink moves. The build runs natively in the VM with the toolchain that
# the fork's rust-toolchain.toml pins. ---
echo "==> forked firecracker"
FC_SRC=third_party/firecracker
FC_SHA="$(git ls-tree HEAD "$FC_SRC" | awk '{print $3}')"
if [ -z "$FC_SHA" ]; then
    echo "fc-colima-provision: no $FC_SRC submodule gitlink at HEAD" >&2
    exit 1
fi
installed_sha="$(ssh_ cat /opt/engram-dev/firecracker.sha 2>/dev/null || true)"
if [ "$installed_sha" = "$FC_SHA" ] && ssh_ test -x /usr/local/bin/firecracker; then
    echo "    fork @ $FC_SHA already installed"
else
    if ! git -C "$FC_SRC" cat-file -e "${FC_SHA}^{commit}" 2>/dev/null; then
        if [ -e "$FC_SRC/.git" ]; then
            echo "    fetching fork commit $FC_SHA into $FC_SRC"
            git -C "$FC_SRC" fetch --quiet origin "$FC_SHA"
        else
            echo "    initializing the $FC_SRC submodule"
            git submodule update --init "$FC_SRC"
        fi
    fi
    echo "    staging fork @ $FC_SHA into the VM (VM-local dir, not virtiofs)"
    git -C "$FC_SRC" archive --format=tar "$FC_SHA" \
        | ssh_ bash -c 'rm -rf /opt/engram-dev/fc-build && mkdir -p /opt/engram-dev/fc-build && tar -C /opt/engram-dev/fc-build -xf -'
    echo "    building the fork for aarch64-unknown-linux-musl in-VM"
    ssh_ bash -c '
        set -euo pipefail
        if [ ! -x "$HOME/.cargo/bin/rustup" ]; then
            curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
                | sh -s -- -y --default-toolchain none --profile minimal --no-modify-path
        fi
        export PATH="$HOME/.cargo/bin:$PATH"
        cd /opt/engram-dev/fc-build
        rustup toolchain install
        CC_aarch64_unknown_linux_musl=musl-gcc \
        BINDGEN_EXTRA_CLANG_ARGS="-I/usr/include -I/usr/include/aarch64-linux-gnu" \
            cargo build -p firecracker --release --target aarch64-unknown-linux-musl
        bin=build/cargo_target/aarch64-unknown-linux-musl/release/firecracker
        file "$bin" | grep -q "statically linked"
        sudo install -m 0755 "$bin" /usr/local/bin/firecracker
    '
    ssh_ bash -c "echo $FC_SHA > /opt/engram-dev/firecracker.sha"
    echo "    removing /opt/engram-dev/fc-build work tree"
    ssh_ sudo rm -rf /opt/engram-dev/fc-build
fi
ssh_ /usr/local/bin/firecracker --version

# --- dm-thin pool for session disks (ADR 0125): the same script node-prep
# runs in production, piped into the VM ---
echo "==> thin pool inside '$PROFILE'"
ssh_ sudo env POOL_DIR=/var/lib/engram/thin POOL_DATA_SIZE=20G POOL_META_SIZE=1G sh -s \
    < deploy/helm/engram-host-fleet/files/thin-pool.sh

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
  - packages: kernel and Firecracker build deps, curl git file iptables squashfs-tools
  - /usr/local/bin/firecracker (fork @ $FC_SHA)
  - vm.unprivileged_userfaultfd=1, /dev/kvm mode 0666 (persisted)
  - dm-thin pool engram-pool (20G data / 1G metadata under /var/lib/engram/thin)
  - engram-dev-fwd.service (DNAT localhost:{5001,4443,4317} -> 192.168.5.2, no
    shadowing loopback listener — see the script comment for why not socat)
  - /opt/engram-dev/{bin,shared,var}

Contract paths:
  /usr/local/bin/firecracker
  /opt/engram-dev/Image
  /opt/engram-dev/{bin,shared,var}

Next: just dev-fc $PROFILE
EOF
