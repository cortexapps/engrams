variable "name_prefix" {
  type        = string
  description = "Prefix for the role name."
}

variable "oidc_provider_arn" {
  type        = string
  description = "The cluster OIDC provider ARN."
}

variable "oidc_provider" {
  type        = string
  description = "The OIDC provider URL without the scheme."
}

variable "namespace" {
  type        = string
  description = "K8s namespace the host-fleet release lives in (PSA-privileged)."
  default     = "engrams-hosts"
}

variable "ksa_name" {
  type        = string
  description = "The operator's K8s ServiceAccount name — the chart derives `<release>-operator`."
}

variable "asg_arn" {
  type        = string
  description = "The fleet ASG's ARN (the kvm-nodegroup module's output) — scopes the mutating actions."
}

variable "tags" {
  type        = map(string)
  description = "Tags on the role."
  default     = {}
}
