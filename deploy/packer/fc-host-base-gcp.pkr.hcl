# Packer manifest for the Engram Firecracker host BASE image on GCE.
#
# This is the slow, OS-layer half of a two-stage bake (see
# fc-host-gcp.pkr.hcl for the thin half). It produces a reusable base
# image — `engram-fc-host-base` — carrying everything that does NOT
# change per host-agent commit:
#
#   - Firecracker binary at /usr/local/bin/firecracker (pinned version)
#   - FC guest kernel at /usr/local/lib/engram/vmlinux
#   - Google Cloud Ops Agent (journal -> Cloud Logging)
#   - OpenTelemetry Collector (otelcol-contrib -> Cloud Trace, ADR 0019)
#   - engram-host-agent.service systemd unit + engram-drain.sh hook
#     (enabled but inert until the binary + env file land)
#   - KVM/NBD module-load + tuning, vm.unprivileged_userfaultfd sysctl
#   - iptables-persistent for FC's per-VM TAP rules
#   - /var/lib/engram working directories
#
# The per-commit thin bake (fc-host-gcp.pkr.hcl) starts FROM this
# family and only drops the two freshly-built binaries on top, so the
# expensive apt/download/config work here runs only when the OS layer
# actually changes (detect-rebake-lanes.py's `host_base` lane:
# deploy/packer/, deploy/otel/, FC/kernel/otel versions).
#
# Build with:
#   packer init deploy/packer/fc-host-base-gcp.pkr.hcl
#   packer build \
#     -var "project_id=$PROJECT" \
#     -var "image_family=engram-fc-host-base" \
#     deploy/packer/fc-host-base-gcp.pkr.hcl

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
  description = "Base image family to publish under. The thin bake consumes this as its source_image_family."
  default     = "engram-fc-host-base"
}

variable "source_image_family" {
  type        = string
  description = "Stock base image family. debian-12 is what the FC test images target."
  default     = "debian-12"
}

variable "source_image_project_id" {
  type        = list(string)
  description = "Projects searched (in order) for `source_image_family`. debian-cloud for the stock Debian base."
  default     = ["debian-cloud"]
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

variable "gh_token" {
  type        = string
  sensitive   = true
  description = "GitHub token with read access to cortexapps/engrams releases — install-fc-kernel.sh fetches the private FC guest-kernel asset (ADR 0025) on the build VM via the releases API (curl + python3, both installed in step 1). Empty = the script falls back to a pre-staged file or anonymous fetch."
  default     = ""
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

source "googlecompute" "fc_host_base" {
  project_id              = var.project_id
  zone                    = var.zone
  region                  = var.region
  source_image_family     = var.source_image_family
  source_image_project_id = var.source_image_project_id
  image_family            = var.image_family
  image_name              = "${var.image_family}-{{timestamp}}"
  image_description       = "Engram Firecracker host BASE image (FC ${var.firecracker_version})"
  machine_type            = var.machine_type
  network                 = var.network
  subnetwork              = var.subnetwork
  disk_size               = 50
  # pd-ssd: the base bake is apt/download/image-snapshot IO-bound, so the
  # faster disk shaves a little off an already-infrequent build.
  disk_type       = "pd-ssd"
  ssh_username    = "packer"
  use_internal_ip = false
  # Ops Agent + otelcol install via curl/apt and are enabled-but-not-started
  # at bake time, so no GCP API calls happen on the build VM. cloud-platform
  # is kept as harmless headroom for any future bake-time GCP probe.
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
  name = "fc-host-base"

  sources = [
    "source.googlecompute.fc_host_base",
  ]

  # 1. Base apt packages. Tooling for KVM probing, iptables persistence,
  #    rootfs packing. NOTE: no google-cloud-cli — binaries are no longer
  #    pulled via gsutil on the build VM (the thin bake uploads them over
  #    SSH), and nothing else here needs gcloud.
  #
  # Inline provisioners below all set `inline_shebang` to bash: Packer's
  # default `/bin/sh -e` resolves to dash on Debian, which doesn't support
  # `set -o pipefail`. Bash is in the base image so the swap is free.
  provisioner "shell" {
    inline_shebang = "/usr/bin/env bash"
    inline = [
      "set -euo pipefail",
      "sudo apt-get update -y",
      "sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \\",
      "    ca-certificates curl gnupg lsb-release \\",
      "    iptables iptables-persistent netfilter-persistent \\",
      "    qemu-utils e2fsprogs cpu-checker python3",
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

  # 2b. FC guest kernel at /usr/local/lib/engram/vmlinux. Host-agent boots
  # every microVM with this kernel; without it `create()` rejects every
  # sandbox spec. ADR 0025: this is engram's own kernel (nf_tables + raw table
  # for in-guest Docker), a private GitHub release asset — install-fc-kernel.sh
  # fetches it on the VM via the releases API using GH_TOKEN.
  provisioner "shell" {
    environment_vars = [
      "GH_TOKEN=${var.gh_token}",
    ]
    script = "${path.root}/provisioners/install-fc-kernel.sh"
  }

  # 3. Google Cloud Ops Agent. Ships engram-host-agent's systemd journal
  #    (plus the rest of the system journal: kernel, OOM, iptables) to
  #    Cloud Logging. GCP-specific — installer + config under
  #    provisioners/gcp/.
  provisioner "shell" {
    inline_shebang = "/usr/bin/env bash"
    script         = "${path.root}/provisioners/gcp/install-ops-agent.sh"
  }

  # 4. OpenTelemetry Collector (ADR 0019) — local trace collector. Receives
  #    OTLP on :4317 from host-agent (localhost) + in-guest agentd (TAP
  #    gateway) and ships traces to Cloud Trace via the googlecloud exporter.
  #    Stage the shared config + the systemd unit, then install.
  provisioner "file" {
    source      = "${path.root}/../otel/collector-gcp.yaml"
    destination = "/tmp/collector-gcp.yaml"
  }
  provisioner "file" {
    source      = "${path.root}/provisioners/systemd/otelcol.service"
    destination = "/tmp/otelcol.service"
  }
  provisioner "shell" {
    inline_shebang = "/usr/bin/env bash"
    script         = "${path.root}/provisioners/gcp/install-otelcol.sh"
  }

  # 5. engram-host-agent systemd unit + drain hook. The unit is `enable`d
  #    here but won't start during the bake (or on first boot) until the
  #    binary lands (thin bake) AND the MIG startup-script drops the env
  #    file. `systemctl enable` only creates the WantedBy symlink, so the
  #    absent binary is fine at base-bake time.
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

  # 6. KVM verification + module load on next boot, NBD tuning, and the
  #    UFFD sysctl. cpu-checker's `kvm-ok` reports the bare CPU's
  #    capability; the build worker may lack nested-virt, so we don't fail
  #    the build — the IMAGE runs on real KVM-capable hosts.
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
      # ADR 0007 Phase 4: the NBD daemon allocates from a fixed pool of
      # /dev/nbdN devices, sized at modprobe time. 64 gives comfortable
      # headroom for high-density hosts; operators reduce via
      # /etc/modprobe.d/engram-nbd-tuning.conf if they want fewer slots.
      "echo 'options nbd nbds_max=64' | sudo tee /etc/modprobe.d/engram-nbd-tuning.conf",
      # The host-agent runs as root so default 0660 permissions are fine;
      # document the device name format for operators who later want to
      # scope it to a non-root user.
      "echo '# ADR 0007 Phase 4 NBD devices created by `nbd` module' | sudo tee /etc/udev/rules.d/90-engram-nbd.rules",
      "echo 'KERNEL==\"nbd*\", GROUP=\"root\", MODE=\"0660\"' | sudo tee -a /etc/udev/rules.d/90-engram-nbd.rules",
      # Firecracker creates the guest's userfaultfd and SCM_RIGHTS-passes the
      # fd to engram-uffd-handler. When FC runs jailed (non-root uid without
      # CAP_SYS_PTRACE) the kernel gates userfaultfd(2) behind
      # vm.unprivileged_userfaultfd. Enable it persistently so UFFD restore
      # works under the jailer (ADR 0007/0020). Review before baking if your
      # host's threat model restricts unprivileged userfaultfd.
      "echo 'vm.unprivileged_userfaultfd = 1' | sudo tee /etc/sysctl.d/60-engram-uffd.conf",
    ]
  }

  # 7. Engram-specific directories + permissions.
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
      # The host-agent runs as root (CAP_NET_ADMIN for TAPs + /dev/kvm for
      # Firecracker). The systemd unit locks it down with ProtectSystem etc.
      "sudo chmod 0750 /var/lib/engram",
    ]
  }

  # 8. Final sanity-check. The binaries are NOT present yet — they bake in
  #    on top of this family via fc-host-gcp.pkr.hcl.
  provisioner "shell" {
    inline_shebang = "/usr/bin/env bash"
    inline = [
      "set -euo pipefail",
      "/usr/local/bin/firecracker --version",
      "test -f /usr/local/lib/engram/vmlinux",
      "echo 'base image build OK'",
    ]
  }
}
