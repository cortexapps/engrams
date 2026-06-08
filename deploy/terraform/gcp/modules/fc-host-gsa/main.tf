# ADR 0044 K5: the per-host Google Service Account — extracted from the retired
# fc-host-mig module. The GCE Firecracker MIG is gone, but this identity lives
# on: the Kubernetes host-fleet pods impersonate it via Workload Identity (the
# engram-host-fleet chart's SA annotation + the host_fleet_wi binding), and the
# KEK / chunks-bucket / Secret Manager grants are bound to it at the caller.
# Identity only — no compute.

resource "google_service_account" "fc_host" {
  account_id   = var.instance_sa_account_id
  display_name = "Engram FC host instance SA"
}

resource "google_project_iam_member" "log_writer" {
  project = var.project_id
  role    = "roles/logging.logWriter"
  member  = "serviceAccount:${google_service_account.fc_host.email}"
}

resource "google_project_iam_member" "metric_writer" {
  project = var.project_id
  role    = "roles/monitoring.metricWriter"
  member  = "serviceAccount:${google_service_account.fc_host.email}"
}

# ADR 0019: traces to Cloud Trace, authing as this SA. Granted only when
# tracing is on.
resource "google_project_iam_member" "cloudtrace_agent" {
  count   = var.otel_collector_endpoint == "" ? 0 : 1
  project = var.project_id
  role    = "roles/cloudtrace.agent"
  member  = "serviceAccount:${google_service_account.fc_host.email}"
}

# Optional: read-only on Artifact Registry so the host-agent can pull harness
# packs + images by URI. Skipped if the caller doesn't pass an AR repo.
resource "google_artifact_registry_repository_iam_member" "ar_reader" {
  count      = var.artifact_registry_repo_id == "" ? 0 : 1
  project    = var.project_id
  location   = var.region
  repository = var.artifact_registry_repo_id
  role       = "roles/artifactregistry.reader"
  member     = "serviceAccount:${google_service_account.fc_host.email}"
}
