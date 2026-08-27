variable "name_prefix" {
  type        = string
  description = "Prefix for the secret ids (e.g. `engram` → `engram-kek-master`)."
}

variable "accessors" {
  type        = map(list(string))
  description = <<-EOT
    Per-shell accessor grants: shell name → IAM members. Shell names:
    kek-master, auth-tokens, better-auth-secret, egress-ca-cert,
    egress-ca-key. The production shape grants the ESO relay GSA on
    every shell, the coordinator GSA on kek-master + auth-tokens, and
    the host GSA on auth-tokens + the CA pair.
  EOT
  default     = {}
}
