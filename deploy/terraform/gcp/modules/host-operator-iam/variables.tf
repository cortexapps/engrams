variable "project_id" {
  type        = string
  description = "GCP project id."
}

variable "name_prefix" {
  type        = string
  description = "Prefix for the GSA account id (e.g. `engram`)."
}

variable "role_id" {
  type        = string
  description = "Custom role id (project-unique)."
  default     = "engramHostOperator"
}

variable "namespace" {
  type        = string
  description = "K8s namespace the host-fleet release lives in (PSA-privileged)."
  default     = "engrams-hosts"
}

variable "ksa_name" {
  type        = string
  description = "The operator's K8s ServiceAccount name — the chart creates `<release>-operator` (e.g. release `hf` → `hf-operator`)."
}
