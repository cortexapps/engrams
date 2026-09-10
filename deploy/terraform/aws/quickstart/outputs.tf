output "engram_values" {
  description = "TF-derived values overlay for the engram chart: `terraform output -raw engram_values > engram.tfvalues.yaml`."
  value = templatefile("${path.module}/templates/engram-values.yaml.tpl", {
    app_namespace        = var.app_namespace
    bucket               = module.storage.bucket_name
    region               = var.region
    kek_key_arn          = aws_kms_key.kek.arn
    coordinator_ksa      = var.coordinator_ksa
    coordinator_role_arn = module.irsa_coordinator.role_arn
    cert_arn             = aws_acm_certificate.web.arn
    domain               = var.domain
    admin_email          = var.admin_email
  })
}

output "host_fleet_values" {
  description = "TF-derived values overlay for the engram-host-fleet chart: `terraform output -raw host_fleet_values > host-fleet.tfvalues.yaml`."
  value = templatefile("${path.module}/templates/host-fleet-values.yaml.tpl", {
    fleet_namespace     = var.fleet_namespace
    bucket              = module.storage.bucket_name
    region              = var.region
    host_fleet_role_arn = module.irsa_host_fleet.role_arn
    ca_cert_secret      = module.secret_shells.secret_names["egress-ca-cert"]
    ca_key_secret       = module.secret_shells.secret_names["egress-ca-key"]
    operator_role_arn   = module.irsa_host_operator.operator_role_arn
    asg_name            = module.kvm_nodegroup.asg_name
  })
}

output "chunks_bucket" {
  value       = module.storage.bucket_name
  description = "The blob-tier bucket."
}

output "kek_key_arn" {
  value       = aws_kms_key.kek.arn
  description = "The KMS KEK (kek.provider=aws-kms)."
}

output "secret_shell_names" {
  value       = module.secret_shells.secret_names
  description = "Populate these (docs/deploy-aws.md carries the exact commands) BEFORE installing the Helm releases."
}

output "acm_certificate_arn" {
  value       = aws_acm_certificate.web.arn
  description = "The web certificate — poll `aws acm describe-certificate` on it until Status is ISSUED (docs/deploy-aws.md step 4)."
}

output "acm_validation_records" {
  value = [
    for dvo in aws_acm_certificate.web.domain_validation_options : {
      name   = dvo.resource_record_name
      type   = dvo.resource_record_type
      record = dvo.resource_record_value
    }
  ]
  description = "Create these DNS records to validate the ACM cert when `route53_zone_id` is empty."
}

output "asg_name" {
  value       = module.kvm_nodegroup.asg_name
  description = "The fleet ASG — the operator's autoscaling.nodePool."
}

output "cluster_name" {
  value       = module.eks_cluster.cluster_name
  description = "For `aws eks update-kubeconfig --name <cluster>`."
}
