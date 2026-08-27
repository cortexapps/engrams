variable "cluster_name" {
  type        = string
  description = "GKE cluster to attach the pool to (the gke-cluster module's output)."
}

variable "region" {
  type        = string
  description = "Cluster location (region)."
}

variable "name" {
  type        = string
  description = "Node-pool name. This is also the operator's `autoscaling.nodePool` value in the host-fleet chart."
  default     = "engram-kvm"
}

variable "node_locations" {
  type        = list(string)
  description = "Zones for the pool. Production runs a single zone (same-zone evacuation); null lets GKE spread across the region."
  default     = null
}

variable "initial_node_count" {
  type        = number
  description = "Seed size only — the ADR 0048 operator owns the count afterwards (ignore_changes)."
  default     = 2
}

variable "machine_type" {
  type        = string
  description = "Intel machine type with nested virtualization (C3 = Sapphire Rapids, production-validated). AMD (C3D) can never work."
  default     = "c3-standard-22"
}

variable "boot_disk_type" {
  type        = string
  description = "Boot-disk type. hyperdisk-balanced carries the fleet where the family has no local-SSD shapes."
  default     = "hyperdisk-balanced"
}

variable "boot_disk_size_gb" {
  type        = number
  description = "Boot-disk size. Work dir + chunk cache + snapshots live here (ADR 0070 sizes the cache from the filesystem)."
  default     = 750
}

variable "boot_disk_provisioned_iops" {
  type        = number
  description = "Provisioned IOPS (hyperdisk only; null for pd-* types)."
  default     = 25000
}

variable "boot_disk_provisioned_throughput" {
  type        = number
  description = "Provisioned throughput MB/s (hyperdisk only; null for pd-* types)."
  default     = 1200
}

variable "labels" {
  type        = map(string)
  description = "Resource labels applied to the pool's instances + disks (cost attribution)."
  default     = {}
}

variable "extra_node_labels" {
  type        = map(string)
  description = "Extra K8s node labels beside the fixed `engram.io/kvm=true` (e.g. a pool-generation label for staged machine-family migrations)."
  default     = {}
}
