output "cluster_name" {
  value       = google_container_cluster.primary.name
  description = "Cluster name (feed to the gke-kvm-pool module and kubectl credentials)."
}

output "cluster_location" {
  value       = google_container_cluster.primary.location
  description = "Cluster location (region)."
}

output "cluster_endpoint" {
  value       = google_container_cluster.primary.endpoint
  description = "Public control-plane endpoint."
}

output "cluster_ca_certificate" {
  value       = google_container_cluster.primary.master_auth[0].cluster_ca_certificate
  description = "Base64 CA cert — configures the kubernetes/helm providers in a composing root."
}

output "workload_identity_pool" {
  value       = "${var.project_id}.svc.id.goog"
  description = "The Workload Identity pool KSA↔GSA bindings reference."
}
