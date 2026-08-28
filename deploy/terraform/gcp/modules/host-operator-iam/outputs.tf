output "operator_sa_email" {
  value       = google_service_account.host_operator.email
  description = "Set as the host-fleet chart's `operator.serviceAccount.annotations.\"iam.gke.io/gcp-service-account\"`."
}
