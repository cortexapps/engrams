output "asg_name" {
  value       = aws_autoscaling_group.kvm.name
  description = "Set as the host-fleet chart's `operator.autoscaling.nodePool` (the `asg` scaler's pool identifier)."
}

output "asg_arn" {
  value       = aws_autoscaling_group.kvm.arn
  description = "For scoping the operator's IAM policy."
}

output "node_role_name" {
  value       = aws_iam_role.node.name
  description = "The minimal node role (worker/CNI/ECR-read only — workload access rides IRSA)."
}

output "node_role_arn" {
  value       = aws_iam_role.node.arn
  description = "For the cluster's aws-auth / access entries."
}
