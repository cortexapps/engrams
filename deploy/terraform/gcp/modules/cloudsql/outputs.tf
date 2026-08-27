output "instance_name" {
  value       = google_sql_database_instance.engram.name
  description = "Instance name — extra databases (e.g. a telemetry store) attach here from the caller."
}

output "instance_connection_name" {
  value       = google_sql_database_instance.engram.connection_name
  description = "For the Cloud SQL Auth Proxy."
}

output "private_ip_address" {
  value       = google_sql_database_instance.engram.private_ip_address
  description = "In-VPC address the DSNs point at."
}

output "database_url_secret_id" {
  value       = google_secret_manager_secret.database_url.secret_id
  description = "Secret Manager id of the coordinator DSN (sslmode=require)."
}

output "orchestrator_database_url_secret_id" {
  value       = google_secret_manager_secret.orchestrator_database_url.secret_id
  description = "Secret Manager id of the orchestrator DSN (sslmode=no-verify — the node-pg posture)."
}

output "db_user_name" {
  value       = google_sql_user.engram.name
  description = "The application SQL user."
}
