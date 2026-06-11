output "instance_sa_email" {
  description = "Per-host GSA email — bind KMS / Secret Manager / bucket grants to this."
  value       = google_service_account.fc_host.email
}

output "instance_sa_id" {
  description = "Per-host GSA resource id."
  value       = google_service_account.fc_host.id
}
