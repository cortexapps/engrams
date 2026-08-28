variable "name_prefix" {
  type        = string
  description = "Prefix for the instance + secret names."
}

variable "vpc_id" {
  type        = string
  description = "VPC id."
}

variable "subnet_ids" {
  type        = list(string)
  description = "Private subnets for the DB subnet group."
}

variable "allowed_security_group_ids" {
  type        = list(string)
  description = "Security groups allowed to reach 5432 (the cluster node/pod SGs)."
}

variable "engine_version" {
  type        = string
  description = "Postgres major.minor."
  default     = "16.6"
}

variable "instance_class" {
  type        = string
  description = "Instance class."
  default     = "db.t4g.medium"
}

variable "db_user_name" {
  type        = string
  description = "The application SQL user (shared by coordinator + orchestrator)."
  default     = "engram"
}

variable "multi_az" {
  type        = bool
  description = "Multi-AZ standby — promote once you have an SLA."
  default     = false
}

variable "deletion_protection" {
  type        = bool
  description = "Refuse destroy + take a final snapshot. Flip to true once stable."
  default     = false
}

variable "secret_recovery_window_in_days" {
  type        = number
  description = "Secrets Manager deletion recovery window for the DSN secrets."
  default     = 0
}

variable "tags" {
  type        = map(string)
  description = "Tags on every resource."
  default     = {}
}
