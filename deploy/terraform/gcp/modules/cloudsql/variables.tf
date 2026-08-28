variable "project_id" {
  type        = string
  description = "GCP project id."
}

variable "region" {
  type        = string
  description = "Instance region."
}

variable "name_prefix" {
  type        = string
  description = "Prefix for the instance + secret names (e.g. `engram`)."
}

variable "network_name" {
  type        = string
  description = "VPC network name (the `network` module's output) for the private-IP peering."
}

variable "tier" {
  type        = string
  description = "Custom machine tier (ENTERPRISE edition accepts db-custom-*)."
  default     = "db-custom-2-7680"
}

variable "availability_type" {
  type        = string
  description = "ZONAL (starting shape) or REGIONAL (HA) — promote once you have an SLA."
  default     = "ZONAL"
}

variable "enable_public_ip" {
  type        = bool
  description = "Keep the public IP for the Cloud SQL Auth Proxy path (no authorized networks — direct connections stay blocked)."
  default     = true
}

variable "db_user_name" {
  type        = string
  description = "The application SQL user (shared by coordinator + orchestrator)."
  default     = "engram"
}

variable "deletion_protection" {
  type        = bool
  description = "Refuse `terraform destroy` of the instance. Flip to true once stable."
  default     = false
}

variable "labels" {
  type        = map(string)
  description = "User labels on the instance (cost attribution)."
  default     = {}
}

variable "secret_accessor_members" {
  type        = list(string)
  description = "IAM members (e.g. `serviceAccount:eso@...`) granted secretAccessor on both DSN secrets."
  default     = []
}
