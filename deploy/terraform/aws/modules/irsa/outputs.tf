output "role_arn" {
  value       = aws_iam_role.this.arn
  description = "Set as the KSA's `eks.amazonaws.com/role-arn` annotation."
}

output "role_name" {
  value       = aws_iam_role.this.name
  description = "For additional attachments from the caller."
}
