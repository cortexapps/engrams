# AWS Secrets Manager shells for the operator-owned secrets — the
# AWS twin of gcp/modules/secret-shells (ADR 0122).
#
# Entries are *created empty* by Terraform (no version resource) and
# populated out of band so secret material never enters tfstate. (The
# DSN secrets are the documented exception and live in the `rds`
# module — Terraform generates that password.)
#
# Names use the AWS-idiomatic slash namespace `<prefix>/<name>` —
# Secrets Manager names allow `/`, so IAM prefix policies
# (`<prefix>/*`) scope cleanly, and the coordinator's default
# `aws-sm://` resolution (`engram/<repo>/<NAME>`) shares the prefix.
#
# Populate after `terraform apply`:
#
#   # KEK master lives in KMS on AWS (kek.provider=aws-kms), so no
#   # KEK shell exists here — the two clouds differ on purpose.
#
#   # Auth tokens — comma-separated bearer allow-list
#   aws secretsmanager put-secret-value \
#     --secret-id <prefix>/auth-tokens \
#     --secret-string "$(openssl rand -hex 32)"
#
#   # better-auth secret — the orchestrator's session-signing key
#   aws secretsmanager put-secret-value \
#     --secret-id <prefix>/better-auth-secret \
#     --secret-string "$(openssl rand -base64 48)"
#
#   # Egress CA pair — generate once, ten-year cert
#   openssl req -x509 -newkey rsa:4096 -nodes \
#     -keyout /tmp/ca.key -out /tmp/ca.pem -days 3650 \
#     -subj "/CN=Engram Egress Proxy CA"
#   aws secretsmanager put-secret-value \
#     --secret-id <prefix>/egress-ca-cert --secret-string file:///tmp/ca.pem
#   aws secretsmanager put-secret-value \
#     --secret-id <prefix>/egress-ca-key --secret-string file:///tmp/ca.key
#   rm /tmp/ca.key /tmp/ca.pem
#
# Installing the Helm releases BEFORE populating these leaves the
# relayed K8s Secrets unsynced and the pods CrashLooping — populate
# first, or re-sync afterwards.

locals {
  shells = {
    "auth-tokens"        = "Comma-separated coordinator bearer allow-list (the first entry doubles as CONTROL_PLANE_BEARER)."
    "better-auth-secret" = "The orchestrator's better-auth session-signing secret."
    "kek-master"         = "Base64 32-byte KEK for the orchestrator's in-process sealing (ADR 0051). The coordinator's KEK is the KMS key; the orchestrator has no KMS path."
    "egress-ca-cert"     = "Egress proxy CA certificate PEM (fleet-wide — ADR 0006)."
    "egress-ca-key"      = "Egress proxy CA private key PEM."
  }
}

resource "aws_secretsmanager_secret" "shell" {
  for_each = local.shells

  name        = "${var.name_prefix}/${each.key}"
  description = each.value
  # 0 = delete immediately on destroy (no 7-30 day recovery window
  # squatting the name on a re-apply during bring-up). Raise for prod.
  recovery_window_in_days = var.recovery_window_in_days

  tags = var.tags
}
