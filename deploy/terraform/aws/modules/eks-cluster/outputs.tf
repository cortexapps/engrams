output "cluster_name" {
  value       = module.eks.cluster_name
  description = "Cluster name."
}

output "cluster_endpoint" {
  value       = module.eks.cluster_endpoint
  description = "API server endpoint (providers + nodeadm join)."
}

output "cluster_certificate_authority_data" {
  value       = module.eks.cluster_certificate_authority_data
  description = "Base64 cluster CA (providers + nodeadm join)."
}

output "cluster_service_cidr" {
  value       = module.eks.cluster_service_cidr
  description = "Service CIDR (nodeadm join)."
}

output "cluster_version" {
  value       = module.eks.cluster_version
  description = "Resolved K8s version (selects the kvm-nodegroup AMI)."
}

output "oidc_provider" {
  value       = module.eks.oidc_provider
  description = "OIDC provider URL without scheme — the irsa module's `oidc_provider`."
}

output "oidc_provider_arn" {
  value       = module.eks.oidc_provider_arn
  description = "OIDC provider ARN — the irsa module's `oidc_provider_arn`."
}

output "node_security_group_id" {
  value       = module.eks.node_security_group_id
  description = "Shared node SG — the kvm-nodegroup attaches it, and RDS allows it."
}
