output "secret_ids" {
  value       = { for name, s in google_secret_manager_secret.shell : name => s.secret_id }
  description = "Shell name → Secret Manager secret_id (feed to ExternalSecret specs / populate commands)."
}
