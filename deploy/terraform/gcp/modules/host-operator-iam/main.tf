# ADR 0048: the host-fleet OPERATOR's cloud identity — promoted from
# the production deployment (ADR 0122). A dedicated GSA with a
# least-privilege custom role for GKE node-pool autoscaling.
#
# Deliberately separate from BOTH the coordinator GSA (which keeps
# ZERO cloud creds — only the operator ever touches the cloud control
# plane) and the fc-host GSA (chunks/secrets — a different blast
# radius).
#
# The operator's two cloud calls and their IAM permissions (verified
# against `gcloud iam list-testable-permissions` — GKE has NO
# container.nodePools.* family; node pools are sub-resources of the
# cluster):
#   - scale-UP: Container API `nodePools.get` + `nodePools.setSize`
#       → container.clusters.get     (read the pool's MIG URLs)
#       → container.clusters.update  (setSize)
#   - scale-DOWN: Compute `instanceGroupManagers.get` + `.deleteInstances`
#       → compute.instanceGroupManagers.get
#       → compute.instanceGroupManagers.update  (deleteInstances — the
#         MIG deletes the instance on the caller's behalf, so NO
#         compute.instances.delete is needed)
# Nothing on individual instances, disks, secrets, or storage.

resource "google_service_account" "host_operator" {
  account_id   = "${var.name_prefix}-host-operator"
  display_name = "Engram host-fleet operator — GKE node-pool autoscaling (${var.name_prefix})"
}

resource "google_project_iam_custom_role" "host_operator" {
  project     = var.project_id
  role_id     = var.role_id
  title       = "Engram Host Fleet Operator"
  description = "Least-privilege GKE node-pool autoscaling for the engram host-fleet operator (ADR 0048)."
  permissions = [
    "container.clusters.get",
    "container.clusters.update",
    "compute.instanceGroupManagers.get",
    "compute.instanceGroupManagers.update",
  ]
}

resource "google_project_iam_member" "host_operator_role" {
  project = var.project_id
  role    = google_project_iam_custom_role.host_operator.id
  member  = "serviceAccount:${google_service_account.host_operator.email}"
}

# Workload Identity: the operator's K8s SA impersonates this GSA. The
# chart wires the reverse half via `operator.serviceAccount.annotations`
# (iam.gke.io/gcp-service-account = this module's email output).
resource "google_service_account_iam_member" "host_operator_wi" {
  service_account_id = google_service_account.host_operator.name
  role               = "roles/iam.workloadIdentityUser"
  member             = "serviceAccount:${var.project_id}.svc.id.goog[${var.namespace}/${var.ksa_name}]"
}
