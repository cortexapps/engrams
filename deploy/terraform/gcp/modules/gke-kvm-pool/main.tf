# ADR 0044: THE nested-virt node pool for the Kubernetes Firecracker
# host fleet — promoted from the production deployment (ADR 0122).
#
# Everything below that looks opinionated is an invariant, not a
# preference:
#
# - **Nested virtualization is Intel-only on GCE**, and it is set at
#   pool CREATION and not toggleable. C3 (Sapphire Rapids) is the
#   production-validated family after a machine-family benchmark
#   measured c3-standard-22 at 3.7x the n2 guest's per-core compile
#   throughput. C3D is AMD and can never work.
#
# - **CPUID is a one-way door for snapshots.** Snapshots captured on
#   an older platform restore on a newer one (feature superset);
#   the REVERSE does not hold. Moving this pool to an older machine
#   family requires a re-bake of every enabled image. Cross-cloud:
#   the AWS quickstart defaults to m7i (also Sapphire Rapids) for
#   deliberate CPUID parity, so images bake once for both fleets.
#
# - **`auto_upgrade = false`** — GKE must never drain the stateful
#   fleet on its own schedule. Rolls go through the engram
#   host-operator (drain-gated, node by node — ADR 0044 K3).
#
# - **`ignore_changes = [node_count]`** — the ADR 0048 autoscaling
#   operator owns the pool size (`operator.autoscaling` in the
#   host-fleet chart). Terraform seeds the initial count only. Do
#   NOT attach a GKE cluster autoscaler to this pool: sessions are
#   not pods, so it has nothing to react to, and it would fight the
#   operator (the ADR 0048 §4 analysis).
#
# - The **label/taint pair** matches the host-fleet chart's defaults:
#   nodeSelector `engram.io/kvm=true`, toleration for the
#   `engram.io/kvm=true:NO_SCHEDULE` taint. The taint keeps general
#   workloads off KVM-priced nodes.
#
# Storage: some regions offer no C3 local-SSD shapes; the default
# rides a provisioned hyperdisk-balanced boot disk (measured on this
# configuration: park ~21 s for a 24 GiB sequential write, cold-chunk
# qd1 16k pread 447 µs — warm reads never touch the device). Machine
# shapes with local NVMe can instead RAID0 them via the chart's
# `storage.dedicatedDevices`. Hyperdisk supports LIVE reprovisioning:
#   gcloud compute disks update <disk> --provisioned-throughput=2400

resource "google_container_node_pool" "kvm_nodes" {
  name           = var.name
  location       = var.region
  node_locations = var.node_locations
  cluster        = var.cluster_name

  # Initial size only — the ADR 0048 operator owns the pool size.
  node_count = var.initial_node_count

  node_config {
    machine_type = var.machine_type

    # Work dir + chunk cache + snapshots all live here (unless the
    # shape carries local NVMe and the chart stripes it).
    boot_disk {
      disk_type              = var.boot_disk_type
      size_gb                = var.boot_disk_size_gb
      provisioned_iops       = var.boot_disk_provisioned_iops
      provisioned_throughput = var.boot_disk_provisioned_throughput
    }

    resource_labels = merge(var.labels, {
      component = "kvm-hosts"
    })

    advanced_machine_features {
      enable_nested_virtualization = true
      threads_per_core             = 2
    }

    workload_metadata_config {
      mode = "GKE_METADATA"
    }
    oauth_scopes = ["https://www.googleapis.com/auth/cloud-platform"]
    metadata     = { disable-legacy-endpoints = "true" }

    # The host-fleet chart's nodeSelector targets `engram.io/kvm`;
    # extra_node_labels can add pool-generation labels for staged
    # migrations between machine families.
    labels = merge({ "engram.io/kvm" = "true" }, var.extra_node_labels)
    taint {
      key    = "engram.io/kvm"
      value  = "true"
      effect = "NO_SCHEDULE"
    }
  }

  management {
    auto_repair = true
    # Never let GKE drain the stateful fleet — see the header.
    auto_upgrade = false
  }

  lifecycle {
    ignore_changes = [node_count] # the ADR 0048 operator owns the pool size
  }
}
