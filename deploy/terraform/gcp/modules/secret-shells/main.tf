# Secret Manager shells for the deployment's operator-owned secrets —
# promoted from the production deployment (ADR 0122).
#
# Entries are *created empty* by Terraform (no `_version` resource)
# and populated out of band so secret material never enters tfstate.
# (The DSN secrets are the documented exception and live in the
# `cloudsql` module — Terraform generates that password, so the DSN
# is necessarily TF-owned.)
#
# Populate after `terraform apply`:
#
#   # KEK master — 32 bytes base64 (env-var KEK provider)
#   openssl rand -base64 32 | \
#     gcloud secrets versions add <prefix>-kek-master --data-file=- \
#       --project=<project>
#
#   # Auth tokens — comma-separated bearer allow-list
#   echo -n "$(openssl rand -hex 32)" | \
#     gcloud secrets versions add <prefix>-auth-tokens --data-file=- \
#       --project=<project>
#
#   # better-auth secret — the orchestrator's session-signing key
#   openssl rand -base64 48 | \
#     gcloud secrets versions add <prefix>-better-auth-secret --data-file=- \
#       --project=<project>
#
#   # Egress CA pair — generate once, ten-year cert
#   openssl req -x509 -newkey rsa:4096 -nodes \
#     -keyout /tmp/ca.key -out /tmp/ca.pem -days 3650 \
#     -subj "/CN=Engram Egress Proxy CA"
#   gcloud secrets versions add <prefix>-egress-ca-cert --data-file=/tmp/ca.pem
#   gcloud secrets versions add <prefix>-egress-ca-key  --data-file=/tmp/ca.key
#   rm /tmp/ca.key /tmp/ca.pem
#
# Installing the Helm releases BEFORE populating these leaves the
# relayed K8s Secrets unsynced and the pods CrashLooping — populate
# first, or re-sync afterwards.

locals {
  # name => description. One shell each.
  shells = {
    "kek-master"         = "32-byte base64 master key (env-var KEK provider)."
    "auth-tokens"        = "Comma-separated coordinator bearer allow-list (machine path; the first entry doubles as CONTROL_PLANE_BEARER)."
    "better-auth-secret" = "The orchestrator's better-auth session-signing secret."
    "egress-ca-cert"     = "Egress proxy CA certificate PEM (fleet-wide — ADR 0006)."
    "egress-ca-key"      = "Egress proxy CA private key PEM."
  }

  # (shell, member) grant pairs, flattened for for_each.
  grants = merge([
    for name, members in var.accessors : {
      for member in members : "${name}:${member}" => {
        name   = name
        member = member
      }
    }
  ]...)
}

resource "google_secret_manager_secret" "shell" {
  for_each  = local.shells
  secret_id = "${var.name_prefix}-${each.key}"

  replication {
    auto {}
  }
}

resource "google_secret_manager_secret_iam_member" "access" {
  for_each  = local.grants
  secret_id = google_secret_manager_secret.shell[each.value.name].id
  role      = "roles/secretmanager.secretAccessor"
  member    = each.value.member
}
