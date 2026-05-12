variable "project_id" {
  type        = string
  description = "GCP project the MIG runs in."
}

variable "name" {
  type        = string
  description = "Name prefix for the MIG + child resources."
}

variable "region" {
  type        = string
  description = "Region the regional MIG spans. Pick one with >=3 zones for HA."
}

variable "image_family" {
  type        = string
  description = "GCE image family the instance template boots from. Matches the Packer manifest's image_family."
  default     = "engram-fc-host"
}

variable "machine_type" {
  type        = string
  description = "Per-host shape. n2-standard-8 is a sensible default for ~10 sessions/host."
  default     = "n2-standard-8"
}

variable "boot_disk_size_gb" {
  type        = number
  description = "Boot disk size. Default 100 GB gives room for the chunk cache."
  default     = 100
}

variable "boot_disk_type" {
  type        = string
  description = "Boot disk type. pd-balanced is the cost/perf sweet spot; pd-ssd if your sessions are IO-heavy."
  default     = "pd-balanced"
}

variable "network_name" {
  type        = string
  description = "VPC the hosts attach to. Output of the `network` module."
}

variable "subnet_self_link" {
  type        = string
  description = "Subnet self_link the hosts attach to. Output of the `network` module."
}

variable "network_tag" {
  type        = string
  description = "Tag to attach to instances (firewall rule targets, etc.)."
  default     = "engram-fc-host"
}

variable "iap_target_tag" {
  type        = string
  description = "Tag exposing IAP-tunneled SSH ingress. Matches the network module's iap_target_tag output."
  default     = "iap-ssh"
}

variable "assign_public_ip" {
  type        = bool
  description = "Whether to give each host an ephemeral public IP. Off in production (Cloud NAT handles egress)."
  default     = false
}

variable "instance_sa_account_id" {
  type        = string
  description = "Account_id (no domain suffix) for the per-host SA."
  default     = "engram-fc-host"
}

variable "chunks_user_id" {
  type        = string
  description = "Fully-qualified resource id of the chunks-bucket user SA. Output of the `storage` module."
}

variable "chunks_bucket" {
  type        = string
  description = "Name of the GCS chunks bucket. Output of the `storage` module."
}

variable "kernel_image_path" {
  type        = string
  description = "Path inside the host image to the Firecracker kernel image (vmlinux)."
  default     = "/usr/local/lib/engram/vmlinux"
}

variable "coordinator_endpoint" {
  type        = string
  description = "ws:// or wss:// URL the host-agent dials. Internal LB or service mesh entry; never the public ingress."
}

variable "coordinator_token" {
  type        = string
  description = "Bearer token the host-agent sends on its WS upgrade."
  sensitive   = true
  default     = ""
}

variable "warm_pool_size" {
  type        = number
  description = "Default per-image warm-pool slot count this host maintains."
  default     = 2
}

variable "nbd_slots" {
  type        = number
  description = <<-EOT
    ADR 0007 Phase 4. Number of /dev/nbdN devices the host-agent
    allocates from when serving chunked disks via the NBD daemon.
    Must be ≤ `nbds_max` set on the kernel `nbd` module
    (Packer image pins 64). Setting 0 disables NBD and falls back
    to the materialize-to-file path (slower cold start, still
    correct).
  EOT
  default     = 16
}

variable "egress_proxy_port" {
  type        = number
  description = "TCP port the per-host egress proxy binds. 0 disables egress filtering (NOT recommended in prod)."
  default     = 8443
}

variable "egress_ca_source" {
  type        = string
  description = "Where the host-agent loads the egress-proxy CA from. `gcp-secret-manager` for prod, `local-disk` for dev."
  default     = "gcp-secret-manager"
}

variable "egress_ca_gcp_cert_secret" {
  type        = string
  description = "Secret Manager resource path for the CA cert PEM. Required when egress_ca_source=gcp-secret-manager."
  default     = ""
}

variable "egress_ca_gcp_key_secret" {
  type        = string
  description = "Secret Manager resource path for the CA key PEM. Required when egress_ca_source=gcp-secret-manager."
  default     = ""
}

variable "target_size" {
  type        = number
  description = "Initial MIG target. Autoscaler overrides at runtime when enabled."
  default     = 3
}

variable "artifact_registry_repo_id" {
  type        = string
  description = "Artifact Registry repo id the host-agent pulls from. Empty skips the IAM grant."
  default     = ""
}

variable "autoscale" {
  type = object({
    enabled      = bool
    min_replicas = number
    max_replicas = number
    cpu_target   = number
  })
  description = "MIG autoscaler config. cpu_target is fractional (0.6 = 60%)."
  default = {
    enabled      = true
    min_replicas = 2
    max_replicas = 30
    cpu_target   = 0.6
  }
}
