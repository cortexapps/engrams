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
  description = <<-EOT
    `http://` or `https://` URL the host-agent dials. Internal LB
    or service mesh entry; never the public ingress.

    ADR 0013 retired the WebSocket dialer; ADR 0016 §A.1.4 retired
    the `ws://`/`wss://` compat shim. Non-HTTP schemes will be
    rejected by `reqwest` at the first request with a "builder
    error for url" — a clearer failure than the silent rewrite the
    shim used to do.
  EOT
}

variable "coordinator_token" {
  type        = string
  description = "Bearer token the host-agent sends as `Authorization: Bearer ...`."
  sensitive   = true
  default     = ""
}

variable "warm_pool_size" {
  type        = number
  description = "Default per-image warm-pool slot count this host maintains."
  default     = 2
}

variable "warm_pool_disabled" {
  type        = bool
  description = <<-EOT
    When true, host-agent receives `ENGRAM_WARM_POOL_DISABLED=1` and
    forces every per-template target to 0 — no warm slots are ever
    pre-restored on this host and every session takes the cold-create
    path. Today (2026-05-21) flipped on to dodge two stacked warm-
    restore bugs: (1) AMD-baked snapshot CPUID restored on Intel
    Cascade Lake prod hosts puts the guest's glibc ifunc resolver on
    AMD-only AVX-512 paths the underlying Intel CPU can't execute,
    (2) `swap_harness_drive`'s symlink-and-PATCH dance doesn't
    propagate the session's claude harness to the guest's mounted
    `/dev/vdb` view. Cold-create dodges both because the guest boots
    fresh on the prod CPU and constructs the harness substrate at
    create time (no snapshot, no swap). Cost: ~20 s cold-boot latency
    per session (vs. ~1 s warm). Acceptable while the warm-path
    bugs land separately. Set back to `false` once the underlying
    fixes ship.
  EOT
  default     = false
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

variable "chunk_cache_budget_bytes" {
  type        = number
  description = <<-EOT
    Maximum bytes the host-agent's local NVMe-backed chunk cache
    is allowed to consume (`ENGRAM_CHUNK_CACHE_BUDGET_BYTES`). The
    cache lazily evicts past this budget. Defaults to 200 GiB
    inside the binary; this variable is the per-fleet override.
    Set to 0 to leave the binary's default in place.
  EOT
  default     = 0
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

variable "update_max_surge" {
  type        = number
  description = <<-EOT
    `update_policy.max_surge_fixed` for the regional MIG. Regional
    MIGs reject any value that isn't 0 or >= the number of zones in
    the region. `null` (default) → auto-derive to the region's zone
    count (parallel-zonal rolling, no capacity dip; pays for one
    extra host per zone during rollouts). `0` → drain-then-create
    (paired with `max_unavailable = zone_count` derived behind the
    scenes; one zone's worth of capacity drains during each roll).
    Any positive integer → operator-tuned; must be >= zone count.
  EOT
  default     = null
  nullable    = true
}

variable "coord_grpc_source_ranges" {
  type        = list(string)
  default     = []
  description = <<-DESC
    ADR 0013: CIDRs allowed to reach the host-agent's gRPC
    `HostService` on port 9101. In GKE-hosted production this is
    the cluster's pod CIDR (alias IPs), since coord pods dial
    hosts directly with their pod IPs (VPC-native networking
    bypasses node-IP NAT). Leave empty to skip the rule; the
    intra-VPC firewall in the network module still allows traffic
    from the primary CIDR — but GKE pod IPs are typically in a
    secondary range outside that.
  DESC
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

variable "distribution_policy_target_shape" {
  type        = string
  default     = "ANY"
  description = <<-DESC
    Regional MIG zonal placement strategy. One of:
      - EVEN: strict even spread across zones. Best fault isolation,
        but a single zone running out of capacity for `machine_type`
        deadlocks rolling updates — the MIG will keep retrying in
        the stocked-out zone (observed in prod 2026-05-20: us-west2-b
        out of n2-standard-8 for 30+ min, MIG retry loop visible in
        `gcloud compute operations list`).
      - BALANCED: prefers EVEN but tolerates capacity issues by
        landing in any zone with availability. Good middle ground.
      - ANY: opportunistic — picks any zone with capacity, no
        balance constraint. Best capacity availability, weakest
        zonal fault tolerance.
      - ANY_SINGLE_ZONE: pin all instances to one zone (no HA).

    Default ANY because today's fleet is small (2 hosts) and we
    care more about capacity availability during rolling updates
    than about zonal spread. Bump to EVEN once the fleet is large
    enough that one zone losing capacity is a smaller percentage
    of total fleet capacity.
  DESC
}
