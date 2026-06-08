variable "project_id" {
  type        = string
  description = "GCP project the host GSA lives in."
}

variable "instance_sa_account_id" {
  type        = string
  description = "Account_id (no domain suffix) for the per-host SA."
  default     = "engram-fc-host"
}

variable "region" {
  type        = string
  description = "Region for the optional Artifact Registry reader binding."
  default     = ""
}

variable "otel_collector_endpoint" {
  type        = string
  default     = "http://localhost:4317"
  description = <<-DESC
    Non-empty grants the SA `roles/cloudtrace.agent` (ADR 0019). Set to "" to
    skip the binding. The endpoint value itself is consumed by the host-agent,
    not this module — only emptiness gates the grant.
  DESC
}

variable "artifact_registry_repo_id" {
  type        = string
  description = "Artifact Registry repo id the host-agent pulls from. Empty skips the IAM grant."
  default     = ""
}
