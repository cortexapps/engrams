# GKE cluster for the engram control plane (coordinator + web +
# orchestrator pods) — promoted from the production deployment
# (ADR 0122). The nested-virt host pool is the SEPARATE
# `gke-kvm-pool` module; this cluster carries only the small
# general-purpose pool.
#
# The charts silently depend on three cluster-level settings this
# module makes explicit:
# - Workload Identity — the coordinator pod impersonates a GSA via
#   `serviceAccount.annotations` in the Helm values.
# - Managed Prometheus — both charts' PodMonitoring CRDs are inert
#   without it.
# - VPC-native (ip_allocation_policy) — required for Workload
#   Identity and the GKE Ingress path.
#
# Raw google_container_cluster rather than
# `terraform-google-modules/kubernetes-engine`: the google-module is
# great for big estates, but for this starting shape the raw resource
# is clearer to read.

terraform {
  required_providers {
    google-beta = {
      source = "hashicorp/google-beta"
    }
  }
}

resource "google_container_cluster" "primary" {
  provider = google-beta

  name     = var.name
  location = var.region

  network    = var.network_name
  subnetwork = var.subnet_self_link

  # Private nodes (no public IPs), public master endpoint — kubectl
  # works from anywhere; the API server itself is auth-gated. Tighten
  # master_authorized_networks to gate by source IP.
  private_cluster_config {
    enable_private_nodes    = true
    enable_private_endpoint = false
    master_ipv4_cidr_block  = var.master_ipv4_cidr_block
  }

  # Workload Identity is load-bearing (see the header).
  workload_identity_config {
    workload_pool = "${var.project_id}.svc.id.goog"
  }

  # GKE 1.27+ requires either sizing or removing the default pool;
  # remove it and define our own below.
  remove_default_node_pool = true
  initial_node_count       = 1

  release_channel {
    channel = var.release_channel
  }

  resource_labels = var.labels

  # PodMonitoring in both Helm releases sends metrics to Google
  # Managed Service for Prometheus; make the prerequisite explicit.
  dynamic "monitoring_config" {
    for_each = var.enable_managed_prometheus ? [1] : []
    content {
      managed_prometheus {
        enabled = true
      }
    }
  }

  # Optional: the GKE Gateway API (GA CRDs + controller). The base
  # deployment uses classic Ingress and does not need it; enable for
  # Gateway-based extensions (e.g. wildcard preview domains).
  dynamic "gateway_api_config" {
    for_each = var.enable_gateway_api ? [1] : []
    content {
      channel = "CHANNEL_STANDARD"
    }
  }

  # VPC-native with auto-allocated secondary ranges (fewest knobs).
  ip_allocation_policy {}

  deletion_protection = var.deletion_protection

  # Required with Workload Identity + private nodes — otherwise
  # plan/apply churns on `default_max_pods_constraint` drift.
  lifecycle {
    ignore_changes = [
      node_config,
    ]
  }
}

# Small general-purpose pool for the control-plane pods. Nothing the
# coordinator/web/orchestrator runs needs a specialty machine family
# — the FC guests live on the gke-kvm-pool module's nodes.
resource "google_container_node_pool" "primary_nodes" {
  name     = "primary"
  location = var.region
  cluster  = google_container_cluster.primary.name

  node_count = var.primary_pool_node_count # per-zone; regional cluster multiplies by zones

  autoscaling {
    min_node_count = var.primary_pool_min_nodes
    max_node_count = var.primary_pool_max_nodes
  }

  node_config {
    machine_type = var.primary_pool_machine_type
    disk_size_gb = 50
    disk_type    = "pd-balanced"

    resource_labels = merge(var.labels, {
      component = "control-plane-nodes"
    })

    # Workload Identity propagates to the pool — pods mint cloud-API
    # tokens via the GSA without node-level scopes.
    workload_metadata_config {
      mode = "GKE_METADATA"
    }

    oauth_scopes = ["https://www.googleapis.com/auth/cloud-platform"]

    metadata = {
      disable-legacy-endpoints = "true"
    }
  }

  management {
    auto_repair  = true
    auto_upgrade = true
  }
}
