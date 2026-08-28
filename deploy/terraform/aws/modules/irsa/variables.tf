variable "role_name" {
  type        = string
  description = "IAM role name."
}

variable "oidc_provider_arn" {
  type        = string
  description = "The cluster OIDC provider ARN (the eks-cluster module's output)."
}

variable "oidc_provider" {
  type        = string
  description = "The OIDC provider URL WITHOUT the https:// scheme (e.g. oidc.eks.<region>.amazonaws.com/id/XXXX)."
}

variable "namespace" {
  type        = string
  description = "K8s namespace of the ServiceAccount."
}

variable "service_account" {
  type        = string
  description = "K8s ServiceAccount name."
}

variable "policies" {
  type        = map(string)
  description = "Inline policies: name → policy JSON."
  default     = {}
}

variable "managed_policy_arns" {
  type        = list(string)
  description = "Managed policy ARNs to attach."
  default     = []
}

variable "tags" {
  type        = map(string)
  description = "Tags on the role."
  default     = {}
}
