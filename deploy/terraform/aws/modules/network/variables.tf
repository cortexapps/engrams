variable "name" {
  type        = string
  description = "Name prefix for the VPC + child resources."
}

variable "region" {
  type        = string
  description = "Region (forms the S3 endpoint service name)."
}

variable "cidr_block" {
  type        = string
  description = "VPC CIDR."
  default     = "10.10.0.0/16"
}

variable "az_count" {
  type        = number
  description = "Availability zones to span (public + private subnet each)."
  default     = 3
}

variable "tags" {
  type        = map(string)
  description = "Tags merged into every resource."
  default     = {}
}
