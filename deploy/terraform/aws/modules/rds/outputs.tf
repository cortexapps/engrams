output "address" {
  value       = aws_db_instance.this.address
  description = "Instance endpoint hostname."
}

output "database_url_secret_arn" {
  value       = aws_secretsmanager_secret.database_url.arn
  description = "Coordinator DSN secret (sslmode=require)."
}

output "orchestrator_database_url_secret_arn" {
  value       = aws_secretsmanager_secret.orchestrator_database_url.arn
  description = "Orchestrator DSN secret (sslmode=no-verify)."
}

output "database_url_secret_name" {
  value       = aws_secretsmanager_secret.database_url.name
  description = "Name form, for ExternalSecret remoteRefs."
}

output "orchestrator_database_url_secret_name" {
  value       = aws_secretsmanager_secret.orchestrator_database_url.name
  description = "Name form, for ExternalSecret remoteRefs."
}

output "master_user" {
  value       = var.db_user_name
  description = "The application SQL user."
}

output "master_password" {
  value       = random_password.master.result
  sensitive   = true
  description = "The application user's password — consumed by the quickstart's one-shot DB-init Job (TF owns the password lifecycle; see the module header)."
}
