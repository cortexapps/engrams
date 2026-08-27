variable "project_id" {
  type        = string
  description = "GCP project id (forms the Workload Identity pool `<project>.svc.id.goog`)."
}

variable "region" {
  type        = string
  description = "Region for the regional cluster."
}

variable "name" {
  type        = string
  description = "Cluster name."
}

variable "network_name" {
  type        = string
  description = "VPC network name (the `network` module's output)."
}

variable "subnet_self_link" {
  type        = string
  description = "Subnetwork self link (the `network` module's output)."
}

variable "master_ipv4_cidr_block" {
  type        = string
  description = "RFC-1918 /28 for the private control plane."
  default     = "172.16.0.0/28"
}

variable "release_channel" {
  type        = string
  description = "GKE release channel."
  default     = "REGULAR"
}

variable "labels" {
  type        = map(string)
  description = "Resource labels applied to the cluster and node pools (cost attribution)."
  default     = {}
}

variable "enable_managed_prometheus" {
  type        = bool
  description = "Google Managed Prometheus — required for the charts' PodMonitoring CRDs to do anything."
  default     = true
}

variable "enable_gateway_api" {
  type        = bool
  description = "Install the GKE Gateway API (STANDARD channel). The base deployment does not need it."
  default     = false
}

variable "deletion_protection" {
  type        = bool
  description = "Refuse `terraform destroy` of the cluster. Flip to true once stable."
  default     = false
}

variable "primary_pool_machine_type" {
  type        = string
  description = "Machine type for the general-purpose control-plane pool."
  default     = "e2-standard-4"
}

variable "primary_pool_node_count" {
  type        = number
  description = "Initial per-zone node count for the primary pool."
  default     = 1
}

variable "primary_pool_min_nodes" {
  type        = number
  description = "Autoscaler floor (per zone) for the primary pool."
  default     = 1
}

variable "primary_pool_max_nodes" {
  type        = number
  description = "Autoscaler ceiling (per zone) for the primary pool."
  default     = 3
}
