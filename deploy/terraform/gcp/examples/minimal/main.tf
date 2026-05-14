# Minimal end-to-end Engram deployment on GCP.
#
# What this builds:
# - A dedicated VPC with NAT.
# - A GCS bucket for ADR 0007 chunks + manifests.
# - A KMS key the coord uses to envelope-encrypt registry creds.
# - A regional MIG of Firecracker hosts.
# - Wiring (IAM, SAs) so the coord (deployed separately via Helm)
#   can authenticate against GCS + KMS via Workload Identity.
#
# What this does NOT build:
# - The GKE cluster the coord runs on. Use Google's published
#   `terraform-google-modules/kubernetes-engine/google` module
#   (it knows about Workload Identity setup + the autopilot vs
#   standard tradeoff better than we'd reinvent here).
# - Cloud SQL for Postgres. Use the published `cloud-sql` module
#   or BYO. The coord just needs DATABASE_URL.
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
  bucket_name        = "${var.name_prefix}-chunks-${random_string.suffix.result}"
  location           = var.region
  user_sa_account_id = "${var.name_prefix}-chunks-user"
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

# Reserved internal IP for the coord's K8s Service of type
# LoadBalancer (the `<release>-coordinator-internal` Service in the
# Helm chart). Reserving the address in Terraform lets us pass it to
# both the FC host MIG (as coordinator_endpoint) AND to Helm (via
# `serviceInternal.loadBalancerIP`) before the cluster comes up, so
# the bring-up is one `terraform apply` + one `helm install` with no
# circular dependency.
#
# `SHARED_LOADBALANCER_VIP` is the right purpose for a GCE internal
# LB consumed by a K8s Service. The address sits idle until Helm
# binds the Service to it; once bound, FC host-agents that have been
# retrying their dial connect on the next attempt.
resource "google_compute_address" "coord_internal" {
  name         = "${var.name_prefix}-coord-internal"
  region       = var.region
  subnetwork   = module.network.subnet_self_link
  address_type = "INTERNAL"
  purpose      = "SHARED_LOADBALANCER_VIP"
}

locals {
  coordinator_endpoint = "ws://${google_compute_address.coord_internal.address}:${var.coordinator_port}"
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

module "fc_host_mig" {
  source = "../../modules/fc-host-mig"

  project_id           = var.project_id
  name                 = "${var.name_prefix}-fc"
  region               = var.region
  network_name         = module.network.network_name
  subnet_self_link     = module.network.subnet_self_link
  iap_target_tag       = module.network.iap_target_tag
  machine_type         = var.host_machine_type
  chunks_user_id       = module.storage.chunks_user_id
  chunks_bucket        = module.storage.bucket_name
  coordinator_endpoint = local.coordinator_endpoint
  coordinator_token    = var.coordinator_token
  target_size          = var.host_count
}

# Host instance SA also needs Encrypt/Decrypt on the KEK for the
# legacy seal path. Tied here rather than inside fc-host-mig so
# the dependency on the kms key is explicit.
resource "google_kms_crypto_key_iam_member" "host_kek_user" {
  crypto_key_id = google_kms_crypto_key.kek.id
  role          = "roles/cloudkms.cryptoKeyEncrypterDecrypter"
  member        = "serviceAccount:${module.fc_host_mig.instance_sa_email}"
}
