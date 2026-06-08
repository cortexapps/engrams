output "chunks_bucket" {
  description = "Set in the Helm chart's values.yaml as blob.gcs.bucket."
  value       = module.storage.bucket_name
}

output "kek_resource" {
  description = "Set in the Helm chart's values.yaml as kek.gcpResource."
  value       = google_kms_crypto_key.kek.id
}

output "coordinator_sa_email" {
  description = "Annotate the coordinator's KSA with `iam.gke.io/gcp-service-account: <this>`."
  value       = google_service_account.coordinator.email
}

output "fc_host_instance_sa_email" {
  description = "Per-host GSA — annotate the engram-host-fleet chart's serviceAccount with this for Workload Identity."
  value       = module.fc_host_gsa.instance_sa_email
}

output "coordinator_internal_lb_ip" {
  description = "Reserved internal IP for the coord's K8s Service of type LoadBalancer. Pass to Helm as `serviceInternal.loadBalancerIP`."
  value       = google_compute_address.coord_internal.address
}

output "coordinator_endpoint" {
  description = "Full `http://` URL the host fleet dials. Use it to point CLI smoke-tests at the same internal LB."
  value       = local.coordinator_endpoint
}

output "network_name" {
  description = "VPC name — pass to your GKE cluster module so the coord pods share the network."
  value       = module.network.network_name
}

output "subnet_self_link" {
  description = "Subnet self link — pass to your GKE cluster module."
  value       = module.network.subnet_self_link
}
