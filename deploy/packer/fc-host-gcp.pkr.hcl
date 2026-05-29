# Packer manifest for the Engram Firecracker host image on GCE — THIN bake.
#
# This is the fast, per-commit half of a two-stage bake. It starts FROM the
# `engram-fc-host-base` family (built by fc-host-base-gcp.pkr.hcl, which
# carries Firecracker, the guest kernel, Ops Agent, otelcol, the systemd
# unit, KVM/NBD config, etc.) and does nothing but drop the two freshly-built
# binaries on top:
#
#   - engram-host-agent  -> /usr/local/bin/engram-host-agent
#   - engram-uffd-handler -> /usr/local/bin/engram-uffd-handler
#
# The binaries are uploaded over SSH from the CI runner's local disk via the
# `file` provisioner — no gsutil, no GCS staging bucket, no google-cloud-cli.
# CI pulls them from GHCR (oras) where the OSS bake-images workflow published
# the exact musl binaries it compiled and tested. So a host-agent change
# skips the entire OS-layer bake; only this ~minute of provisioning + the
# GCE image-snapshot floor remains.
#
# Build with:
#   packer init deploy/packer/fc-host-gcp.pkr.hcl
#   packer build \
#     -var "project_id=$PROJECT" \
#     -var "host_agent_local_path=./engram-host-agent" \
#     -var "uffd_handler_local_path=./engram-uffd-handler" \
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
  description = "Base image family this thin bake layers onto. Produced by fc-host-base-gcp.pkr.hcl."
  default     = "engram-fc-host-base"
}

variable "source_image_project_id" {
  type        = list(string)
  description = "Project(s) the base image family lives in. Defaults to project_id since engram-fc-host-base is one of our own images, not a stock public base."
  default     = null
}

variable "host_agent_local_path" {
  type        = string
  description = "Local filesystem path to the pre-built static engram-host-agent binary (CI pulls it from GHCR before invoking Packer)."
}

variable "uffd_handler_local_path" {
  type        = string
  description = "Local filesystem path to the pre-built static engram-uffd-handler binary (ADR 0020 Route B; spawned by host-agent on a UFFD restore)."
}

variable "firecracker_version" {
  type        = string
  description = "Firecracker release tag — for the image label only; the binary already lives in the base image."
  default     = "v1.10.1"
}

variable "machine_type" {
  type        = string
  description = "Build worker shape. n2-standard-2 is plenty; the thin bake is dominated by VM boot + image snapshot, not CPU."
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
  project_id          = var.project_id
  zone                = var.zone
  region              = var.region
  source_image_family = var.source_image_family
  # The base family is one of our own images. When source_image_project_id
  # is left null, default the search to our own project rather than
  # debian-cloud.
  source_image_project_id = var.source_image_project_id != null ? var.source_image_project_id : [var.project_id]
  image_family            = var.image_family
  image_name              = "${var.image_family}-{{timestamp}}"
  image_description       = "Engram Firecracker host image (FC ${var.firecracker_version})"
  machine_type            = var.machine_type
  network                 = var.network
  subnetwork              = var.subnetwork
  disk_size               = 50
  disk_type               = "pd-ssd"
  ssh_username            = "packer"
  use_internal_ip         = false
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

  # 1. Upload the two pre-built musl-static binaries from the CI runner's
  #    local disk to the build VM over SSH. No download happens on the VM.
  provisioner "file" {
    source      = var.host_agent_local_path
    destination = "/tmp/engram-host-agent"
  }
  provisioner "file" {
    source      = var.uffd_handler_local_path
    destination = "/tmp/engram-uffd-handler"
  }

  # 2. Install both into /usr/local/bin. The systemd unit (baked into the
  #    base image, already `enable`d) picks up engram-host-agent on the next
  #    boot of a real FC host once the MIG startup-script drops the env file.
  provisioner "shell" {
    inline_shebang = "/usr/bin/env bash"
    inline = [
      "set -euo pipefail",
      "file /tmp/engram-host-agent /tmp/engram-uffd-handler",
      "sudo install -m 0755 /tmp/engram-host-agent  /usr/local/bin/engram-host-agent",
      "sudo install -m 0755 /tmp/engram-uffd-handler /usr/local/bin/engram-uffd-handler",
    ]
  }

  # 3. Final sanity-check + bake. Firecracker comes from the base image;
  #    the host-agent binary is what we just installed.
  provisioner "shell" {
    inline_shebang = "/usr/bin/env bash"
    inline = [
      "set -euo pipefail",
      "/usr/local/bin/firecracker --version",
      "/usr/local/bin/engram-host-agent --help >/dev/null",
      "/usr/local/bin/engram-uffd-handler --help >/dev/null 2>&1 || true",
      "echo 'image build OK'",
    ]
  }
}
