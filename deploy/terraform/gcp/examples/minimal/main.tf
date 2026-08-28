# Minimal BYO-cluster Engram footprint on GCP. For the full
# zero-to-running shape, use ../../quickstart instead.
#
# What this builds:
# - A dedicated VPC with NAT.
# - A GCS bucket for ADR 0007 chunks + manifests.
# - A KMS key the coord uses to envelope-encrypt registry creds.
# - Identities + IAM (coordinator GSA, the FC hosts' GSA) so the
#   Helm-deployed workloads can authenticate against GCS + KMS via
#   Workload Identity.
#
# What this does NOT build:
# - The GKE cluster + the nested-virt host pool. Use the sibling
#   `gke-cluster` + `gke-kvm-pool` modules against this VPC (or
#   Google's published `terraform-google-modules/kubernetes-engine`
#   for the cluster — the KVM pool's invariants still want our
#   module).
# - Cloud SQL for Postgres. Use the sibling `cloudsql` module, the
#   published `sql-db` module, or BYO — the coord just needs
#   DATABASE_URL.
# - Artifact Registry. One `google_artifact_registry_repository`
#   resource — small enough that operators inline it per env.
# - Secret Manager entries for the coord's auth tokens / KEK.
#   Operator-specific contents; expose names via outputs and
#   they're populated outside Terraform.
#
# Apply with:
#   terraform init
#   cp terraform.tfvars.example terraform.tfvars && $EDITOR terraform.tfvars
#   terraform plan
#   terraform apply

resource "random_string" "suffix" {
  length  = 6
  upper   = false
  numeric = true
  special = false
}

module "network" {
  source = "../../modules/network"

  name   = "${var.name_prefix}-net"
  region = var.region
}

module "storage" {
  source = "../../modules/storage"

  # GCS bucket names must be globally unique; suffix with random
  # so two test deploys don't collide.
  bucket_name = "${var.name_prefix}-chunks-${random_string.suffix.result}"
  location    = var.region
  labels = {
    env = var.name_prefix
  }
}

# KMS keyring + key the coord uses for envelope encryption. Tied
# to the region so reads stay local.
resource "google_kms_key_ring" "engram" {
  name     = "${var.name_prefix}-keyring"
  location = var.region
}

resource "google_kms_crypto_key" "kek" {
  name            = "${var.name_prefix}-kek"
  key_ring        = google_kms_key_ring.engram.id
  rotation_period = "7776000s" # 90 days

  lifecycle {
    prevent_destroy = true
  }
}

# Coord's Workload Identity binding lives in Helm (chart wires
# `iam.gke.io/gcp-service-account` annotation). We surface the
# GSAs the chart's values.yaml refers to.
resource "google_service_account" "coordinator" {
  account_id   = "${var.name_prefix}-coordinator"
  display_name = "Engram coordinator (${var.name_prefix})"
}

# Reserved internal IP for the coord's OPTIONAL internal-LB Service
# (`serviceInternal` in the Helm chart — for a host fleet OUTSIDE the
# cluster). The ADR 0044 in-cluster DaemonSet fleet dials the
# ClusterIP Service instead and needs none of this; keep it only for
# an out-of-cluster fleet, where reserving the address up front lets
# the fleet's coordinator_endpoint be plumbed before Helm binds the
# Service (one apply + one install, no circular dependency).
#
# `SHARED_LOADBALANCER_VIP` is the right purpose for a GCE internal
# LB consumed by a K8s Service. The address sits idle until Helm
# binds the Service to it.
resource "google_compute_address" "coord_internal" {
  name         = "${var.name_prefix}-coord-internal"
  region       = var.region
  subnetwork   = module.network.subnet_self_link
  address_type = "INTERNAL"
  purpose      = "SHARED_LOADBALANCER_VIP"
}

locals {
  # http, not ws: ADR 0013 retired the WS dial — host-agents register
  # + heartbeat over plain HTTP POSTs.
  coordinator_endpoint = "http://${google_compute_address.coord_internal.address}:${var.coordinator_port}"
}

# Coord needs:
# - read+write on the chunks bucket (so it can run GC + bake-push)
# - Encrypt/Decrypt on the KEK
# - secretAccessor on Secret Manager (covered separately when
#   you add registry creds).
resource "google_storage_bucket_iam_member" "coord_chunks_rw" {
  bucket = module.storage.bucket_name
  role   = "roles/storage.objectAdmin"
  member = "serviceAccount:${google_service_account.coordinator.email}"
}

resource "google_kms_crypto_key_iam_member" "coord_kek_user" {
  crypto_key_id = google_kms_crypto_key.kek.id
  role          = "roles/cloudkms.cryptoKeyEncrypterDecrypter"
  member        = "serviceAccount:${google_service_account.coordinator.email}"
}

# ADR 0044: the Firecracker host fleet runs as a Kubernetes DaemonSet (the
# `engram-host-fleet` Helm chart), not a Terraform GCE MIG. This module creates
# only the per-host GSA the host pods impersonate via Workload Identity —
# annotate the chart's serviceAccount with `fc_host_instance_sa_email` (output
# below). The chunks-bucket + KEK grants below bind to that SA.
module "fc_host_gsa" {
  source = "../../modules/fc-host-gsa"

  project_id = var.project_id
  region     = var.region
}

# Host instance SA needs objectAdmin on the chunks bucket — the
# host-agent reads chunks + writes session-time snapshots through
# GCS. ADC on GCE uses the instance SA directly, so this grant has
# to name `instance_sa_email` (no impersonation chain).
resource "google_storage_bucket_iam_member" "host_chunks_rw" {
  bucket = module.storage.bucket_name
  role   = "roles/storage.objectAdmin"
  member = "serviceAccount:${module.fc_host_gsa.instance_sa_email}"
}

# Host instance SA also needs Encrypt/Decrypt on the KEK for the
# legacy seal path. Tied here rather than inside fc-host-mig so
# the dependency on the kms key is explicit.
resource "google_kms_crypto_key_iam_member" "host_kek_user" {
  crypto_key_id = google_kms_crypto_key.kek.id
  role          = "roles/cloudkms.cryptoKeyEncrypterDecrypter"
  member        = "serviceAccount:${module.fc_host_gsa.instance_sa_email}"
}
