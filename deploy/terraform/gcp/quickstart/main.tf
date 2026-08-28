# GCP quickstart (ADR 0122): zero → an engram-ready cluster in one
# apply. Composes every module under ../modules plus the glue only a
# concrete deployment can decide (namespaces, identity bindings, the
# External Secrets relay, the web certificate).
#
# What ONE `terraform apply` here provisions:
#   network, chunks bucket, GKE cluster + the nested-virt KVM pool,
#   Cloud SQL (controlplane + orchestrator DBs, DSNs in Secret
#   Manager), the operator-populated secret shells, all four service
#   identities (coordinator / fc-host / operator / ESO) with their
#   Workload Identity bindings, both namespaces (the fleet one
#   PSA-privileged), the External Secrets Operator + the three
#   ExternalSecrets the charts read, and the web static IP + managed
#   certificate.
#
# What it deliberately does NOT do (see docs/deploy-gcp.md):
#   populate the secret shells (material never enters tfstate), the
#   DNS record when `dns_zone_name` is empty, and the two
#   `helm install` commands — the `engram_values` / `host_fleet_values`
#   outputs render the TF-derived halves of those values files.

locals {
  labels = merge(var.labels, { app = "engram" })
}

# ─── network + storage ────────────────────────────────────────────

module "network" {
  source = "../modules/network"

  name   = var.name_prefix
  region = var.region
}

resource "random_id" "bucket_suffix" {
  byte_length = 3
}

module "storage" {
  source = "../modules/storage"

  bucket_name = "${var.name_prefix}-durable-storage-${random_id.bucket_suffix.hex}"
  location    = var.region
  labels      = local.labels
}

# ─── cluster + KVM pool ───────────────────────────────────────────

module "gke_cluster" {
  source = "../modules/gke-cluster"

  project_id       = var.project_id
  region           = var.region
  name             = var.name_prefix
  network_name     = module.network.network_name
  subnet_self_link = module.network.subnet_self_link
  labels           = local.labels
}

module "gke_kvm_pool" {
  source = "../modules/gke-kvm-pool"

  cluster_name       = module.gke_cluster.cluster_name
  region             = var.region
  name               = "${var.name_prefix}-kvm"
  machine_type       = var.kvm_machine_type
  node_locations     = var.kvm_node_locations
  initial_node_count = var.kvm_initial_node_count
  labels             = local.labels
}

# ─── identities ───────────────────────────────────────────────────

# The coordinator GSA. Zero cloud-control-plane permissions on
# purpose — bucket + secrets only; the OPERATOR is the one identity
# that touches node pools (ADR 0048).
resource "google_service_account" "coordinator" {
  account_id   = "${var.name_prefix}-coordinator"
  display_name = "Engram coordinator (${var.name_prefix})"
}

resource "google_storage_bucket_iam_member" "coord_chunks_rw" {
  bucket = module.storage.bucket_name
  role   = "roles/storage.objectAdmin"
  member = "serviceAccount:${google_service_account.coordinator.email}"
}

resource "google_service_account_iam_member" "coord_wi" {
  service_account_id = google_service_account.coordinator.name
  role               = "roles/iam.workloadIdentityUser"
  member             = "serviceAccount:${var.project_id}.svc.id.goog[${var.app_namespace}/${var.coordinator_ksa}]"
}

# The FC host identity (chunks + secrets — a different blast radius
# from the coordinator).
module "fc_host_gsa" {
  source = "../modules/fc-host-gsa"

  project_id             = var.project_id
  region                 = var.region
  instance_sa_account_id = "${var.name_prefix}-fc-host"
}

resource "google_storage_bucket_iam_member" "host_chunks_rw" {
  bucket = module.storage.bucket_name
  role   = "roles/storage.objectAdmin"
  member = "serviceAccount:${module.fc_host_gsa.instance_sa_email}"
}

resource "google_service_account_iam_member" "host_fleet_wi" {
  service_account_id = module.fc_host_gsa.instance_sa_id
  role               = "roles/iam.workloadIdentityUser"
  member             = "serviceAccount:${var.project_id}.svc.id.goog[${var.fleet_namespace}/${var.host_fleet_ksa}]"
}

# The autoscaling operator's identity (the only one with cloud
# control-plane permissions).
module "host_operator_iam" {
  source = "../modules/host-operator-iam"

  project_id  = var.project_id
  name_prefix = var.name_prefix
  namespace   = var.fleet_namespace
  ksa_name    = var.operator_ksa
}

# ─── database + secret shells ─────────────────────────────────────

module "cloudsql" {
  source = "../modules/cloudsql"

  project_id   = var.project_id
  region       = var.region
  name_prefix  = var.name_prefix
  network_name = module.network.network_name
  tier         = var.cloudsql_tier
  labels       = local.labels

  # ESO relays the DSNs into the cluster.
  secret_accessor_members = [
    "serviceAccount:${google_service_account.eso.email}",
  ]
}

module "secret_shells" {
  source = "../modules/secret-shells"

  name_prefix = var.name_prefix

  accessors = {
    # ESO relays everything the charts read as K8s Secrets.
    "kek-master"         = ["serviceAccount:${google_service_account.eso.email}"]
    "better-auth-secret" = ["serviceAccount:${google_service_account.eso.email}"]
    "auth-tokens"        = ["serviceAccount:${google_service_account.eso.email}"]
    "egress-ca-cert"     = ["serviceAccount:${google_service_account.eso.email}"]
    "egress-ca-key"      = ["serviceAccount:${google_service_account.eso.email}"]
  }
}

# ─── namespaces ───────────────────────────────────────────────────

resource "kubernetes_namespace_v1" "app" {
  metadata {
    name   = var.app_namespace
    labels = local.labels
  }

  depends_on = [module.gke_cluster]
}

# ADR 0044 hard constraint: the host-agent DaemonSet is privileged +
# hostPID + hostNetwork; the namespace must enforce the `privileged`
# Pod Security level or every fleet pod is rejected at admission.
resource "kubernetes_namespace_v1" "fleet" {
  metadata {
    name = var.fleet_namespace
    labels = merge(local.labels, {
      "pod-security.kubernetes.io/enforce" = "privileged"
    })
  }

  depends_on = [module.gke_cluster]
}

# ─── web static IP + managed certificate ──────────────────────────

resource "google_compute_global_address" "web" {
  name = "${var.name_prefix}-web-ip"
}

# The name matches the values-gcp example's
# `networking.gke.io/managed-certificates: engram-web-cert`
# annotation. Provisioning completes only after DNS resolves to the
# static IP (expect ~15 minutes after the record lands).
resource "kubectl_manifest" "web_managed_cert" {
  yaml_body = <<-YAML
    apiVersion: networking.gke.io/v1
    kind: ManagedCertificate
    metadata:
      name: ${var.name_prefix}-web-cert
      namespace: ${kubernetes_namespace_v1.app.metadata[0].name}
    spec:
      domains:
        - ${var.domain}
  YAML
}

resource "google_dns_record_set" "web" {
  count = var.dns_zone_name == "" ? 0 : 1

  managed_zone = var.dns_zone_name
  name         = "${var.domain}."
  type         = "A"
  ttl          = 300
  rrdatas      = [google_compute_global_address.web.address]
}
