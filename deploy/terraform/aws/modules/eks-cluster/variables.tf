variable "name" {
  type        = string
  description = "Cluster name."
}

variable "cluster_version" {
  type        = string
  description = "K8s version."
  default     = "1.31"
}

variable "vpc_id" {
  type        = string
  description = "VPC id (the network module's output)."
}

variable "private_subnet_ids" {
  type        = list(string)
  description = "Private subnets for the cluster + node group."
}

variable "control_plane_instance_type" {
  type        = string
  description = "Instance type for the general-purpose control-plane node group."
  default     = "m6i.large"
}

variable "control_plane_min_nodes" {
  type        = number
  default     = 2
  description = "Managed node group floor."
}

variable "control_plane_max_nodes" {
  type        = number
  default     = 4
  description = "Managed node group ceiling."
}

variable "control_plane_desired_nodes" {
  type        = number
  default     = 2
  description = "Managed node group start size."
}

variable "tags" {
  type        = map(string)
  description = "Tags on every resource."
  default     = {}
}
