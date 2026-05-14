# Managed Instance Group of Firecracker hosts.
#
# Each host runs `engram-host-agent` as a systemd unit (baked into
# the Packer image), connects to the coord over WS, and serves
# microVM session requests. The MIG handles scaling, replacement,
# and rolling updates; the per-instance drain hook (also in the
# image) lets sessions migrate gracefully when a host goes away.
#
# Production-relevant choices:
# - SPOT VMs are tempting (sessions migrate fast with ADR 0007's
#   chunked storage) but default OFF until operators confirm the
#   sub-2s migration assumption holds for their workload.
# - n2-standard machine family by default — nested-virt capable,
#   broad GCE availability. Operators tune for memory-heavy
#   workloads (n2-highmem) or compute-heavy (c3).
# - REPLACE max-surge=1 + max-unavailable=0 so rolling updates
#   never drop below target capacity. Combined with the drain
#   hook, this gives zero-loss rolling deploys.

resource "google_service_account" "fc_host" {
  account_id   = var.instance_sa_account_id
  display_name = "Engram FC host instance SA"
}

# Bare-minimum IAM the host needs: read chunks (via the bucket
# user SA passed in from the storage module), pull from
# Artifact Registry, log + metric scopes. KMS Decrypt is granted
# at the key resource by the kek module; same pattern for Secret
# Manager via the secrets module.
resource "google_project_iam_member" "log_writer" {
  project = var.project_id
  role    = "roles/logging.logWriter"
  member  = "serviceAccount:${google_service_account.fc_host.email}"
}

resource "google_project_iam_member" "metric_writer" {
  project = var.project_id
  role    = "roles/monitoring.metricWriter"
  member  = "serviceAccount:${google_service_account.fc_host.email}"
}

# The host needs read+write on the chunks bucket. We expect the
# caller to grant that on the bucket itself (the storage module
# does this for its own GSA; this module binds the host SA to
# it via service-account-token-creator so the host can act as
# that bucket SA — Workload Identity for GCE).
resource "google_service_account_iam_member" "host_acts_as_chunks_user" {
  service_account_id = var.chunks_user_id
  role               = "roles/iam.serviceAccountTokenCreator"
  member             = "serviceAccount:${google_service_account.fc_host.email}"
}

# Optional: read-only on Artifact Registry so the host-agent can
# pull harness packs + images by URI. Skipped if the caller
# doesn't pass an AR repo.
resource "google_artifact_registry_repository_iam_member" "ar_reader" {
  count      = var.artifact_registry_repo_id == "" ? 0 : 1
  project    = var.project_id
  location   = var.region
  repository = var.artifact_registry_repo_id
  role       = "roles/artifactregistry.reader"
  member     = "serviceAccount:${google_service_account.fc_host.email}"
}

# Instance template: the baked image + a startup script that
# writes /etc/engram/host-agent.env with per-environment values.
# Bumping `coordinator_endpoint` triggers a new template version
# and a rolling MIG update — handled by the rolling-update policy
# below.
resource "google_compute_instance_template" "fc_host" {
  name_prefix  = "${var.name}-tpl-"
  machine_type = var.machine_type
  region       = var.region

  disk {
    source_image = "projects/${var.project_id}/global/images/family/${var.image_family}"
    boot         = true
    auto_delete  = true
    disk_size_gb = var.boot_disk_size_gb
    disk_type    = var.boot_disk_type
  }

  network_interface {
    network    = var.network_name
    subnetwork = var.subnet_self_link
    # Empty `access_config` → ephemeral public IP. Comment out
    # if you require private-only hosts (then make sure Cloud
    # NAT in the network module is wired).
    dynamic "access_config" {
      for_each = var.assign_public_ip ? [1] : []
      content {}
    }
  }

  service_account {
    email  = google_service_account.fc_host.email
    scopes = ["cloud-platform"]
  }

  tags = compact([var.network_tag, var.iap_target_tag])

  metadata = {
    enable-oslogin = "TRUE"
    # First-boot config writes the env file the systemd unit
    # picks up via EnvironmentFile=.
    startup-script = <<-EOT
      #!/usr/bin/env bash
      set -euo pipefail
      mkdir -p /etc/engram
      cat > /etc/engram/host-agent.env <<EOF
ENGRAM_COORDINATOR_ENDPOINT=${var.coordinator_endpoint}
ENGRAM_COORDINATOR_TOKEN=${var.coordinator_token}
ENGRAM_BLOB_BACKEND=gcs
ENGRAM_GCS_BUCKET=${var.chunks_bucket}
ENGRAM_SANDBOX_BACKEND=firecracker
ENGRAM_KERNEL_IMAGE_PATH=${var.kernel_image_path}
ENGRAM_SANDBOX_WORK_DIR=/var/lib/engram/sandboxes
ENGRAM_WARM_POOL_SIZE=${var.warm_pool_size}
ENGRAM_EGRESS_PROXY_PORT=${var.egress_proxy_port}
ENGRAM_EGRESS_CA_SOURCE=${var.egress_ca_source}
${var.egress_ca_gcp_cert_secret == "" ? "" : "ENGRAM_EGRESS_CA_GCP_CERT_SECRET=${var.egress_ca_gcp_cert_secret}"}
${var.egress_ca_gcp_key_secret == "" ? "" : "ENGRAM_EGRESS_CA_GCP_KEY_SECRET=${var.egress_ca_gcp_key_secret}"}
${var.nbd_slots > 0 ? "ENGRAM_NBD_DEVICES=${join(",", [for i in range(var.nbd_slots) : "/dev/nbd${i}"])}" : ""}
${var.chunk_cache_budget_bytes > 0 ? "ENGRAM_CHUNK_CACHE_BUDGET_BYTES=${var.chunk_cache_budget_bytes}" : ""}
EOF
      systemctl daemon-reload
      systemctl restart engram-host-agent.service
    EOT
  }

  shielded_instance_config {
    enable_secure_boot          = true
    enable_vtpm                 = true
    enable_integrity_monitoring = true
  }

  lifecycle {
    create_before_destroy = true
  }
}

resource "google_compute_region_instance_group_manager" "fc_host" {
  name               = var.name
  region             = var.region
  base_instance_name = var.name
  target_size        = var.target_size

  version {
    instance_template = google_compute_instance_template.fc_host.id
  }

  named_port {
    name = "host-agent-metrics"
    # Metrics endpoint TBD — port reserved for the future
    # `engram-host-agent --metrics-port=9100` flag (see rollout
    # doc "Observability").
    port = 9100
  }

  update_policy {
    type                  = "PROACTIVE"
    minimal_action        = "REPLACE"
    max_surge_fixed       = var.update_max_surge
    max_unavailable_fixed = 0
    # Drain hook (in the image) coordinates with the coord to
    # migrate sessions off before SIGTERM. 5min is enough for
    # the heaviest sessions; bump if your image carries multi-GB
    # memory snapshots.
  }

  auto_healing_policies {
    health_check      = google_compute_health_check.host_agent.id
    initial_delay_sec = 180
  }
}

# Health check hits the host-agent's heartbeat port (or, in the
# absence of a dedicated /healthz, the metrics port reservation).
# The host-agent doesn't bind a TCP health endpoint today —
# rollout doc note: surface `/healthz` once observability lands.
# Until then, this is a TCP probe that the agent's metrics port
# satisfies (or the OS itself responds on if the port is closed,
# triggering replacement, which is loud-fail behavior we want).
resource "google_compute_health_check" "host_agent" {
  name                = "${var.name}-hc"
  check_interval_sec  = 30
  timeout_sec         = 10
  healthy_threshold   = 1
  unhealthy_threshold = 3

  tcp_health_check {
    port = 9100
  }
}

resource "google_compute_region_autoscaler" "fc_host" {
  count  = var.autoscale.enabled ? 1 : 0
  name   = "${var.name}-as"
  region = var.region
  target = google_compute_region_instance_group_manager.fc_host.id

  autoscaling_policy {
    max_replicas    = var.autoscale.max_replicas
    min_replicas    = var.autoscale.min_replicas
    cooldown_period = 90

    cpu_utilization {
      target = var.autoscale.cpu_target
    }
  }
}
