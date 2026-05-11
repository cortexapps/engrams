output "mig_id" {
  description = "Regional MIG self link."
  value       = google_compute_region_instance_group_manager.fc_host.id
}

output "mig_name" {
  description = "MIG name."
  value       = google_compute_region_instance_group_manager.fc_host.name
}

output "instance_template_id" {
  description = "Current instance template id. Bumping triggers a rolling MIG update."
  value       = google_compute_instance_template.fc_host.id
}

output "instance_sa_email" {
  description = "Per-host GSA email."
  value       = google_service_account.fc_host.email
}

output "instance_sa_id" {
  description = "Per-host GSA resource id — pass to KMS/SecretManager grants in the example."
  value       = google_service_account.fc_host.id
}
