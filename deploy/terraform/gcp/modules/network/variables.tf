variable "name" {
  type        = string
  description = "Name prefix for VPC + child resources."
}

variable "region" {
  type        = string
  description = "Region the primary subnet lands in."
}

variable "primary_cidr" {
  type        = string
  description = "CIDR for the primary subnet."
  default     = "10.10.0.0/16"
}

variable "iap_target_tag" {
  type        = string
  description = "Instance tag that opts a VM into IAP-tunneled SSH ingress."
  default     = "iap-ssh"
}
