#!/usr/bin/env bash
# Idempotent VM-side setup. Re-runnable: skips anything already in place.
# Use after a fresh VM create, or whenever you suspect drift.
#
# Installs (if missing): build-essential, postgres-client, jq, git, gh,
# Docker, Rust (rustup), Nix (Determinate), Firecracker, just (via nix
# shell, system fallback). Adds the user to kvm + docker groups.
set -euo pipefail
. "$(dirname "${BASH_SOURCE[0]}")/config.sh"

ssh_exec() {
  gcloud compute ssh "$GCP_INSTANCE" "${gcloud_args[@]}" --command="$1"
}

ssh_exec '
set -e

need() { command -v "$1" >/dev/null 2>&1; }

echo "=== apt base ==="
if ! need pkg-config || ! need psql || ! need jq || ! need clang; then
  sudo apt-get update -y
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y \
    build-essential pkg-config libssl-dev \
    git jq curl ca-certificates gnupg lsb-release \
    postgresql-client \
    clang libclang-dev
fi

echo "=== docker ==="
if ! need docker; then
  curl -fsSL https://get.docker.com | sudo sh
fi

echo "=== group membership ==="
if ! id -nG | tr " " "\n" | grep -qx kvm;    then sudo usermod -aG kvm    "$USER"; fi
if ! id -nG | tr " " "\n" | grep -qx docker; then sudo usermod -aG docker "$USER"; fi

echo "=== rustup (legacy fallback; nix shell is the real toolchain) ==="
if [ ! -d "$HOME/.cargo" ] && [ ! -e /nix/var/nix/profiles/default/bin/nix ]; then
  curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable
fi

echo "=== nix (Determinate) ==="
if ! need nix; then
  curl --proto "=https" --tlsv1.2 -sSf -L https://install.determinate.systems/nix \
    | sh -s -- install linux --no-confirm
fi

echo "=== gh ==="
if ! need gh; then
  GH_VER=$(curl -s https://api.github.com/repos/cli/cli/releases/latest | jq -r .tag_name | sed s/^v//)
  curl -L "https://github.com/cli/cli/releases/download/v${GH_VER}/gh_${GH_VER}_linux_amd64.tar.gz" -o /tmp/gh.tar.gz
  tar -xzf /tmp/gh.tar.gz -C /tmp
  sudo install -m 0755 "/tmp/gh_${GH_VER}_linux_amd64/bin/gh" /usr/local/bin/gh
fi

echo "=== firecracker ==="
# PIN to the prod line, not `latest`. Prod's packer pins v1.10.1
# (deploy/packer/provisioners/install-firecracker.sh) with guest kernel
# vmlinux-5.10.223; the dev-vm MUST match or FC-version drift makes
# dev-vm behavior diverge from prod/CI. (A `latest` here once installed
# FC v1.15.1, which differed from prod — keep this in lockstep with the
# packer pin.) Override with FC_VER=... if intentionally testing another.
FC_VER="${FC_VER:-v1.10.1}"
if ! need firecracker || [ "$(firecracker --version 2>/dev/null | awk 'NR==1{print $2}')" != "$FC_VER" ]; then
  curl -L "https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VER}/firecracker-${FC_VER}-x86_64.tgz" -o /tmp/fc.tgz
  tar -xzf /tmp/fc.tgz -C /tmp
  sudo install -m 0755 "/tmp/release-${FC_VER}-x86_64/firecracker-${FC_VER}-x86_64" /usr/local/bin/firecracker
  sudo install -m 0755 "/tmp/release-${FC_VER}-x86_64/jailer-${FC_VER}-x86_64" /usr/local/bin/jailer
fi

echo "=== /dev/userfaultfd permissions ==="
# Firecracker 1.10+ opens /dev/userfaultfd directly for UFFD-backed
# snapshot restore. Default mode is 0600 (root only); we relax it via
# a udev rule so non-root users (us) can use UFFD without sudo.
if [ ! -f /etc/udev/rules.d/99-userfaultfd.rules ]; then
  echo "KERNEL==\"userfaultfd\", MODE=\"0666\"" \
    | sudo tee /etc/udev/rules.d/99-userfaultfd.rules > /dev/null
  sudo udevadm control --reload-rules
fi
sudo chmod 0666 /dev/userfaultfd 2>/dev/null || true

echo "=== summary ==="
ls /dev/kvm 2>/dev/null && echo "/dev/kvm: present" || echo "/dev/kvm: MISSING"
ls /dev/userfaultfd 2>/dev/null && echo "/dev/userfaultfd: present" || echo "/dev/userfaultfd: MISSING"
nix --version 2>/dev/null || echo "nix: MISSING"
firecracker --version 2>/dev/null | head -1 || echo "firecracker: MISSING"
gh --version 2>/dev/null | head -1 || echo "gh: MISSING"
docker --version 2>/dev/null || echo "docker: MISSING"
groups
echo "DONE"
'
