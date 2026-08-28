output "secret_arns" {
  value       = { for name, s in aws_secretsmanager_secret.shell : name => s.arn }
  description = "Shell name → ARN (feed to IAM policies + ExternalSecret specs)."
}

output "secret_names" {
  value       = { for name, s in aws_secretsmanager_secret.shell : name => s.name }
  description = "Shell name → full Secrets Manager name (the populate commands + chart values use these)."
}
