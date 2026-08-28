output "operator_role_arn" {
  value       = aws_iam_role.host_operator.arn
  description = "Set as the host-fleet chart's `operator.serviceAccount.annotations.\"eks.amazonaws.com/role-arn\"`."
}
