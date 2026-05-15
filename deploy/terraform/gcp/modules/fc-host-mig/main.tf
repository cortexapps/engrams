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

# Used to default `update_policy.max_surge_fixed` to the region's
# zone count — regional MIGs reject any non-zero max_surge/
# max_unavailable that's smaller than the number of zones.
data "google_compute_zones" "region" {
  region = var.region
}

# Look up the latest image in the family at plan time. This is what
# makes the auto-deploy pipeline work: when the bake-fc-host CI
# workflow lands a new image, the family pointer moves, this data
# source resolves to a new `self_link`, and the instance template
# below sees `source_image` as changed — TF then create-before-
# destroy's the template and the MIG rolls onto it. With the older
# `family/<name>` URI form, GCE resolves the family at instance-
# create time and TF never sees a diff, so the MIG would stay on
# whatever image was current when the template was first created.
#
# Bring-up gotcha: this data source requires at least one image in
# the family. On a fresh deploy, run the `bake-fc-host-image`
# workflow ONCE before the first `terraform apply`, or apply the
# rest of the stack first with the FC MIG opted-out (host_count=0)
# and then bake + re-apply.
data "google_compute_image" "fc_host" {
  family  = var.image_family
  project = var.project_id
}

locals {
  zone_count = length(data.google_compute_zones.region.names)

  # Defaults to parallel-zonal rolling: surge = zone_count, no
  # unavailable. Sessions migrate onto the new hosts before the
  # old ones drain (the drain hook is in the host image).
  # Operator-set values fall through unchanged.
  effective_max_surge       = var.update_max_surge != null ? var.update_max_surge : local.zone_count
  effective_max_unavailable = local.effective_max_surge > 0 ? 0 : local.zone_count
}

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

# The host needs read+write on the chunks bucket. Grant that
# directly at the caller — bind `${instance_sa_email}` (this
# module's output) to `roles/storage.objectAdmin` on the bucket.
# We don't do it here because the bucket lives in a sibling module
# and threading another required input through this one to lift
# the binding into here adds API surface without value.

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
    # Pinned to the data source's resolved self_link (not the
    # `family/<name>` shortcut) so TF sees a diff when a new image
    # lands in the family. See the data block comment above.
    source_image = data.google_compute_image.fc_host.self_link
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
    type           = "PROACTIVE"
    minimal_action = "REPLACE"
    # Regional MIG constraints: each of max_surge / max_unavailable
    # must be 0 OR >= the number of zones in the region. They can't
    # both be 0. Default to parallel-zonal rolling (surge =
    # zone_count, unavailable = 0) so the fleet temporarily grows
    # by one host per zone during rollouts and sessions migrate to
    # the new hosts before the old ones drain. Operators tune via
    # `var.update_max_surge`; `unavailable` is derived to stay
    # GCP-valid.
    max_surge_fixed       = local.effective_max_surge
    max_unavailable_fixed = local.effective_max_unavailable
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

# Without this firewall rule, the GCE health check above never
# reaches port 9100 — the VPC's default deny drops probes from the
# GCE health-check IP ranges and the autohealer marks every
# instance unhealthy after `initial_delay_sec`, rolling the MIG in
# a tight loop indefinitely.
#
# The source ranges are the documented GCE health-check infrastructure
# ranges: https://cloud.google.com/load-balancing/docs/health-check-concepts#ip-ranges
# Targets the instances by `network_tag` (same tag the instance
# template stamps on every VM in this MIG).
resource "google_compute_firewall" "host_agent_healthcheck" {
  name    = "${var.name}-hc-allow"
  network = var.network_name
  project = var.project_id

  source_ranges = ["35.191.0.0/16", "130.211.0.0/22"]
  target_tags   = [var.network_tag]

  allow {
    protocol = "tcp"
    ports    = ["9100"]
  }

  description = "Allow GCE health-check probes to reach the host-agent metrics port"
}

# ADR 0013: coord pods dial the host-agent's gRPC server on 9101
# for `HostService` dispatch (CreateSandbox, ExecStart, Snapshot,
# …). GKE in VPC-native mode gives pods alias IPs from a secondary
# range outside the primary subnet CIDR — so the broad intra-VPC
# firewall in the network module doesn't cover this traffic, and
# without this rule every coord→host gRPC call errors with
# `tcp connect error` at the channel-warm `Ping`.
#
# `count = ...` so callers that don't have a coord pod CIDR to
# pass (single-VPC test fixtures, fresh bring-ups before the GKE
# cluster exists) opt out cleanly.
resource "google_compute_firewall" "host_agent_grpc" {
  count   = length(var.coord_grpc_source_ranges) > 0 ? 1 : 0
  name    = "${var.name}-grpc-allow"
  network = var.network_name
  project = var.project_id

  source_ranges = var.coord_grpc_source_ranges
  target_tags   = [var.network_tag]

  allow {
    protocol = "tcp"
    ports    = ["9101"]
  }

  description = "Allow coord (GKE pod CIDR) to reach host-agent gRPC HostService"
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
