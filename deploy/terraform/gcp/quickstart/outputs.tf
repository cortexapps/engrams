output "engram_values" {
  description = "TF-derived values overlay for the engram chart: `terraform output -raw engram_values > engram.tfvalues.yaml`."
  value = templatefile("${path.module}/templates/engram-values.yaml.tpl", {
    app_namespace        = var.app_namespace
    bucket               = module.storage.bucket_name
    project_id           = var.project_id
    coordinator_ksa      = var.coordinator_ksa
    coordinator_sa_email = google_service_account.coordinator.email
    managed_cert_name    = "${var.name_prefix}-web-cert"
    static_ip_name       = google_compute_global_address.web.name
    domain               = var.domain
    admin_email          = var.admin_email
  })
}

output "host_fleet_values" {
  description = "TF-derived values overlay for the engram-host-fleet chart: `terraform output -raw host_fleet_values > host-fleet.tfvalues.yaml`."
  value = templatefile("${path.module}/templates/host-fleet-values.yaml.tpl", {
    fleet_namespace   = var.fleet_namespace
    bucket            = module.storage.bucket_name
    host_sa_email     = module.fc_host_gsa.instance_sa_email
    operator_sa_email = module.host_operator_iam.operator_sa_email
    kvm_pool_name     = module.gke_kvm_pool.pool_name
  })
}

output "web_static_ip" {
  value       = google_compute_global_address.web.address
  description = "Create the A record for `domain` here when `dns_zone_name` is empty."
}

output "chunks_bucket" {
  value       = module.storage.bucket_name
  description = "The blob-tier bucket."
}

output "secret_shell_ids" {
  value       = module.secret_shells.secret_ids
  description = "Populate these (docs/deploy-gcp.md carries the exact commands) BEFORE installing the Helm releases."
}

output "cloudsql_instance" {
  value       = module.cloudsql.instance_name
  description = "The shared Postgres instance."
}

output "cluster_name" {
  value       = module.gke_cluster.cluster_name
  description = "For `gcloud container clusters get-credentials`."
}
