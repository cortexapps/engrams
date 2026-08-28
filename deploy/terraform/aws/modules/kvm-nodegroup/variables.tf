variable "name" {
  type        = string
  description = "ASG + launch-template name. With scaler=asg this IS the operator's `autoscaling.nodePool` value."
  default     = "engram-kvm"
}

variable "cluster_name" {
  type        = string
  description = "EKS cluster name."
}

variable "cluster_version" {
  type        = string
  description = "K8s version (selects the EKS-optimized AMI)."
}

variable "cluster_endpoint" {
  type        = string
  description = "API server endpoint (nodeadm join)."
}

variable "cluster_ca_data" {
  type        = string
  description = "Base64 cluster CA (nodeadm join)."
}

variable "cluster_service_cidr" {
  type        = string
  description = "The cluster's service CIDR (nodeadm join)."
}

variable "subnet_ids" {
  type        = list(string)
  description = "Private subnets the nodes land in."
}

variable "security_group_ids" {
  type        = list(string)
  description = "Node security groups (the eks-cluster module's node SG)."
}

variable "instance_type" {
  type        = string
  description = "Intel KVM-capable type: Xeon-6 C8i/M8i/R8i virtual shapes (nested virt) or bare metal (*.metal). Default m8i.6xlarge = the GCP c3-standard-22 shape twin; use m7i.metal-24xl for CPUID parity with a GCP C3 fleet (see main.tf header)."
  default     = "m8i.6xlarge"
}

variable "initial_node_count" {
  type        = number
  description = "Seed desired_capacity — the ADR 0048 operator owns it afterwards (ignore_changes)."
  default     = 2
}

variable "min_size" {
  type        = number
  description = "ASG floor (the operator's minHosts should match)."
  default     = 0
}

variable "max_size" {
  type        = number
  description = "ASG ceiling — keep it ABOVE the operator's maxHosts so a grow is never clamped silently."
  default     = 8
}

variable "root_volume_size_gb" {
  type        = number
  description = "Root gp3 volume (work dir + chunk cache + snapshots when local NVMe is unused)."
  default     = 750
}

variable "root_volume_iops" {
  type        = number
  description = "gp3 provisioned IOPS."
  default     = 16000
}

variable "root_volume_throughput" {
  type        = number
  description = "gp3 provisioned throughput MB/s."
  default     = 1000
}

variable "extra_node_labels" {
  type        = string
  description = "Extra kubelet node labels beside engram.io/kvm=true, comma-separated `k=v` pairs (e.g. a pool-generation label)."
  default     = ""
}

variable "tags" {
  type        = map(string)
  description = "Tags on every resource."
  default     = {}
}
