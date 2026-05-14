variable "project_id" {
  type        = string
  description = "GCP project to deploy into."
}

variable "region" {
  type        = string
  description = "Primary region."
  default     = "us-central1"
}

variable "name_prefix" {
  type        = string
  description = "Name prefix for resources. Use the env name (eg `engram-prod`) — keeps `kubectl` + `gcloud` lists readable."
  default     = "engram-dev"
}

variable "coordinator_port" {
  type        = number
  description = "Port the coord's internal LB listens on. Matches the Helm chart's `service.port` (default 8080)."
  default     = 8080
}

variable "coordinator_token" {
  type        = string
  description = "Bearer token host-agents send on WS upgrade. Match the coord's ENGRAM_AUTH_TOKENS."
  sensitive   = true
  default     = ""
}

variable "host_count" {
  type        = number
  description = "Initial FC host fleet target."
  default     = 3
}

variable "host_machine_type" {
  type        = string
  description = "FC host machine type. n2-standard-8 fits ~10 sessions at default sizing."
  default     = "n2-standard-8"
}
