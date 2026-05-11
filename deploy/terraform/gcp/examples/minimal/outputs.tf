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

output "chunks_user_sa_email" {
  description = "Bind to the coord's KSA via WI so the coord can act as this SA when talking to GCS."
  value       = module.storage.chunks_user_email
}

output "fc_host_instance_sa_email" {
  description = "Per-host SA — already attached to MIG instances."
  value       = module.fc_host_mig.instance_sa_email
}

output "fc_host_mig_name" {
  description = "MIG name. `gcloud compute instance-groups managed describe <this>` to see fleet state."
  value       = module.fc_host_mig.mig_name
}

output "network_name" {
  description = "VPC name — pass to your GKE cluster module so the coord pods share the network."
  value       = module.network.network_name
}

output "subnet_self_link" {
  description = "Subnet self link — pass to your GKE cluster module."
  value       = module.network.subnet_self_link
}
