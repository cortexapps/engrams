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

variable "coordinator_endpoint" {
  type        = string
  description = "ws:// or wss:// URL host-agents dial to reach the coord. Internal LB or service-mesh entry — never the public ingress."
  default     = ""
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
