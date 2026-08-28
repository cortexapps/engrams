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

output "workload_identity_pool" {
  value       = "${var.project_id}.svc.id.goog"
  description = "The Workload Identity pool KSA↔GSA bindings reference."
}
