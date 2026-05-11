output "network_id" {
  description = "Self link of the VPC."
  value       = google_compute_network.this.id
}

output "network_name" {
  description = "VPC name; useful when child resources accept name rather than self_link."
  value       = google_compute_network.this.name
}

output "subnet_id" {
  description = "Self link of the primary subnet."
  value       = google_compute_subnetwork.primary.id
}

output "subnet_name" {
  description = "Primary subnet name."
  value       = google_compute_subnetwork.primary.name
}

output "subnet_self_link" {
  description = "Primary subnet self_link — what MIG instance templates consume."
  value       = google_compute_subnetwork.primary.self_link
}

output "iap_target_tag" {
  description = "Echo the IAP tag so callers can attach it to instance templates."
  value       = var.iap_target_tag
}
