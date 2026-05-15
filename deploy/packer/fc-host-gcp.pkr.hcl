# Packer manifest for the Engram Firecracker host image on GCE.
#
# Produces a custom GCE image with:
#   - Firecracker binary at /usr/local/bin/firecracker (pinned version)
#   - engram-host-agent binary at /usr/local/bin/engram-host-agent
#   - engram-drain.sh wrapper at /usr/local/bin/engram-drain.sh
#   - systemd unit `engram-host-agent.service` that runs the agent on
#     boot and fires the drain hook on shutdown
#   - KVM kernel module pre-loaded + the user added to the `kvm` group
#   - iptables-persistent installed so FC's per-VM TAP rules survive
#     reboot
#
# Inputs:
#   - host_agent_gcs_url: gs:// URL of a pre-built engram-host-agent
#     binary (musl-static). Upload via your CI before invoking Packer.
#   - firecracker_version: the FC release to pin. v1.10.x is the
#     current production line.
#   - source_image_family: the GCE base image family
#     (default: debian-12).
#
# Build with:
#   packer init deploy/packer/fc-host-gcp.pkr.hcl
#   packer build \
#     -var "project_id=$PROJECT" \
#     -var "host_agent_gcs_url=gs://engram-artifacts/host-agent/0.1.0/engram-host-agent" \
#     -var "image_family=engram-fc-host" \
#     deploy/packer/fc-host-gcp.pkr.hcl
#
# The resulting image is what `fc-host-mig` consumes as
# `source_image_family` in the Terraform module.

packer {
  required_plugins {
    googlecompute = {
      source  = "github.com/hashicorp/googlecompute"
      version = ">= 1.1.0"
    }
  }
}

variable "project_id" {
  type        = string
  description = "GCP project the image lands in."
}

variable "region" {
  type        = string
  description = "Region for the build worker VM."
  default     = "us-central1"
}

variable "zone" {
  type        = string
  description = "Zone for the build worker VM."
  default     = "us-central1-a"
}

variable "image_family" {
  type        = string
  description = "Image family to publish under. Bumping creates a new image in the same family."
  default     = "engram-fc-host"
}

variable "source_image_family" {
  type        = string
  description = "Base image family. debian-12 is what the FC test images target."
  default     = "debian-12"
}

variable "source_image_project_id" {
  type        = list(string)
  description = "Projects searched (in order) for `source_image_family`. The plugin's `googlecompute` source treats this as a fallback chain; a single-element list is the normal case."
  default     = ["debian-cloud"]
}

variable "host_agent_gcs_url" {
  type        = string
  description = "gs:// URL of the pre-built static engram-host-agent binary."
}

variable "firecracker_version" {
  type        = string
  description = "Firecracker release tag to pin. v1.10.x is the production line."
  default     = "v1.10.1"
}

variable "machine_type" {
  type        = string
  description = "Build worker shape. n2-standard-2 is plenty; the build is IO-bound."
  default     = "n2-standard-2"
}

variable "network" {
  type        = string
  description = "VPC for the build worker. `default` works for one-shot builds."
  default     = "default"
}

variable "subnetwork" {
  type        = string
  description = "Subnet for the build worker. Empty = default subnet for the region."
  default     = ""
}

source "googlecompute" "fc_host" {
  project_id              = var.project_id
  zone                    = var.zone
  region                  = var.region
  source_image_family     = var.source_image_family
  source_image_project_id = var.source_image_project_id
  image_family            = var.image_family
  image_name              = "${var.image_family}-{{timestamp}}"
  image_description       = "Engram Firecracker host image (FC ${var.firecracker_version})"
  machine_type            = var.machine_type
  network                 = var.network
  subnetwork              = var.subnetwork
  disk_size               = 50
  disk_type               = "pd-balanced"
  ssh_username            = "packer"
  use_internal_ip         = false
  # Required scopes for `gsutil cp` of the host-agent binary.
  scopes = [
    "https://www.googleapis.com/auth/cloud-platform",
  ]
  image_labels = {
    builder           = "packer"
    firecracker       = replace(var.firecracker_version, ".", "_")
    image_family_name = var.image_family
  }
}

build {
  name = "fc-host"

  sources = [
    "source.googlecompute.fc_host",
  ]

  # 1. Base apt packages. Pull a small set: tooling for KVM probing,
  #    iptables persistence, gsutil for the host-agent download.
  #
  # Inline provisioners below all set `inline_shebang` to bash:
  # Packer's default `/bin/sh -e` resolves to dash on Debian, which
  # doesn't support `set -o pipefail`. Bash is in the base image so
  # the swap is free.
  provisioner "shell" {
    inline_shebang = "/usr/bin/env bash"
    inline = [
      "set -euo pipefail",
      "sudo apt-get update -y",
      "sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \\",
      "    ca-certificates curl gnupg lsb-release \\",
      "    iptables iptables-persistent netfilter-persistent \\",
      "    qemu-utils e2fsprogs cpu-checker python3 \\",
      "    google-cloud-cli",
      "sudo apt-get clean",
    ]
  }

  # 2. Firecracker — pinned release fetched from GitHub.
  provisioner "shell" {
    environment_vars = [
      "FC_VER=${var.firecracker_version}",
    ]
    script = "${path.root}/provisioners/install-firecracker.sh"
  }

  # 2b. FC guest kernel at /usr/local/lib/engram/vmlinux. Host-agent
  # boots every microVM with this kernel; without it `create()`
  # rejects every sandbox spec.
  provisioner "shell" {
    script = "${path.root}/provisioners/install-fc-kernel.sh"
  }

  # 3. engram-host-agent — pulled from the operator-provided GCS URL.
  provisioner "shell" {
    environment_vars = [
      "HOST_AGENT_GCS_URL=${var.host_agent_gcs_url}",
    ]
    script = "${path.root}/provisioners/install-host-agent.sh"
  }

  # 4. systemd unit + drain hook.
  provisioner "file" {
    source      = "${path.root}/provisioners/systemd/engram-host-agent.service"
    destination = "/tmp/engram-host-agent.service"
  }
  provisioner "file" {
    source      = "${path.root}/provisioners/engram-drain.sh"
    destination = "/tmp/engram-drain.sh"
  }
  provisioner "shell" {
    inline_shebang = "/usr/bin/env bash"
    inline = [
      "set -euo pipefail",
      "sudo install -m 0644 /tmp/engram-host-agent.service /etc/systemd/system/engram-host-agent.service",
      "sudo install -m 0755 /tmp/engram-drain.sh /usr/local/bin/engram-drain.sh",
      "sudo systemctl daemon-reload",
      "sudo systemctl enable engram-host-agent.service",
    ]
  }

  # 5. KVM verification + module load on next boot. cpu-checker's
  #    `kvm-ok` reports the bare CPU's capability; the build worker
  #    must be on a host with nested-virt or KVM. We don't fail the
  #    build if kvm-ok complains here — the IMAGE will run on a real
  #    KVM-capable VM (n2-standard-N etc.) where /dev/kvm is present.
  provisioner "shell" {
    inline_shebang = "/usr/bin/env bash"
    inline = [
      "set -euo pipefail",
      # `sudo` because cpu-checker installs kvm-ok at /usr/sbin/kvm-ok,
      # and Packer's SSH session as the `packer` user has a stripped
      # PATH that doesn't include sbin dirs. sudo's secure_path does.
      "sudo kvm-ok || echo 'kvm-ok complained on the build worker — fine; the resulting image runs on real KVM hosts'",
      # Ensure kvm + nbd modules load at boot on the deployed VM.
      "echo kvm | sudo tee /etc/modules-load.d/engram-kvm.conf",
      "echo nbd | sudo tee /etc/modules-load.d/engram-nbd.conf",
      # ADR 0007 Phase 4: the NBD daemon allocates from a fixed
      # pool of /dev/nbdN devices, sized at modprobe time. 64
      # gives us comfortable headroom for high-density hosts;
      # operators reduce via /etc/modprobe.d/engram-nbd-tuning.conf
      # if they want fewer slots.
      "echo 'options nbd nbds_max=64' | sudo tee /etc/modprobe.d/engram-nbd-tuning.conf",
      # The host-agent runs as root so default 0660 permissions
      # are fine; document the device name format for operators
      # who later want to scope it to a non-root user.
      "echo '# ADR 0007 Phase 4 NBD devices created by `nbd` module' | sudo tee /etc/udev/rules.d/90-engram-nbd.rules",
      "echo 'KERNEL==\"nbd*\", GROUP=\"root\", MODE=\"0660\"' | sudo tee -a /etc/udev/rules.d/90-engram-nbd.rules",
    ]
  }

  # 6. Engram-specific directories + permissions.
  provisioner "shell" {
    inline_shebang = "/usr/bin/env bash"
    inline = [
      "set -euo pipefail",
      # Working directory for sandbox state, materialized rootfs files,
      # OCI cache, chunk cache. Tuned to the host-agent's CLI defaults.
      "sudo mkdir -p /var/lib/engram/sandboxes",
      "sudo mkdir -p /var/lib/engram/chunked-rootfs",
      "sudo mkdir -p /var/lib/engram/chunk-cache",
      "sudo mkdir -p /var/lib/engram/oci-cache",
      "sudo mkdir -p /var/lib/engram/egress-ca",
      # The host-agent runs as root (it needs CAP_NET_ADMIN for TAPs +
      # /dev/kvm for Firecracker). systemd unit will lock it down with
      # ProtectSystem etc.
      "sudo chmod 0750 /var/lib/engram",
    ]
  }

  # 7. Final sanity-check + bake. Verify the binaries are in place;
  #    the systemd unit is masked until the deployed instance has its
  #    env file (provisioned by the MIG's startup-script).
  provisioner "shell" {
    inline_shebang = "/usr/bin/env bash"
    inline = [
      "set -euo pipefail",
      "/usr/local/bin/firecracker --version",
      "/usr/local/bin/engram-host-agent --help >/dev/null",
      "echo 'image build OK'",
    ]
  }
}
