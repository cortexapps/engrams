variable "region" {
  type        = string
  description = "AWS region."
}

variable "name_prefix" {
  type        = string
  description = "Prefix for every named resource."
  default     = "engram"
}

variable "domain" {
  type        = string
  description = "Public domain for the web Ingress (e.g. engrams.example.com). Drives the ACM certificate + the values output; DNS validation + the final CNAME are `route53_zone_id` or manual records."
}

variable "route53_zone_id" {
  type        = string
  description = "Optional: a Route53 hosted zone (this account) — ACM validation records are created automatically. Empty = create the validation records yourself (the `acm_validation_records` output)."
  default     = ""
}

variable "admin_email" {
  type        = string
  description = "Bootstrap admin: promoted to role 'admin' on first sign-in."
}

variable "kvm_instance_type" {
  type        = string
  description = "Intel KVM-capable type (see the kvm-nodegroup module)."
  default     = "m7i.metal-24xl"
}

variable "kvm_initial_node_count" {
  type        = number
  description = "Seed size for the KVM ASG (the operator owns it afterwards)."
  default     = 2
}

variable "app_namespace" {
  type        = string
  description = "Namespace for the engram (control-plane) release."
  default     = "engrams"
}

variable "fleet_namespace" {
  type        = string
  description = "PSA-privileged namespace for the engram-host-fleet release."
  default     = "engrams-hosts"
}

variable "coordinator_ksa" {
  type        = string
  description = "The coordinator's K8s ServiceAccount name (values-aws example: engram-coordinator)."
  default     = "engram-coordinator"
}

variable "host_fleet_ksa" {
  type        = string
  description = "The host-agent DaemonSet's K8s ServiceAccount name — the chart derives `<release>-host-agent` (docs install as release `hf`)."
  default     = "hf-host-agent"
}

variable "operator_ksa" {
  type        = string
  description = "The operator's K8s ServiceAccount name — the chart derives `<release>-operator`."
  default     = "hf-operator"
}

variable "tags" {
  type        = map(string)
  description = "Tags applied to every resource."
  default     = {}
}
