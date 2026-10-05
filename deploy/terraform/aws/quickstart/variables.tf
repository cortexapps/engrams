# The three required inputs are validated NON-EMPTY: `-var region=$REGION`
# with $REGION unset in a fresh shell passes "" (Terraform accepts it, the
# provider silently falls back to the ambient region) and the first AWS
# bring-up applied that — it destroyed the S3 gateway endpoint (service name
# `com.amazonaws..s3`) and wrote a region-less ARN into the coordinator's
# IAM policy. Step 2 of docs/deploy-aws.md writes terraform.tfvars once so
# later applies carry no flags at all.
variable "region" {
  type        = string
  description = "AWS region (e.g. us-west-2)."

  validation {
    condition     = can(regex("^[a-z]{2}(-[a-z]+)+-[0-9]$", var.region))
    error_message = "region must be an AWS region id like us-west-2 (empty means $REGION was unset)."
  }
}

variable "name_prefix" {
  type        = string
  description = "Prefix for every named resource."
  default     = "engram"
}

variable "domain" {
  type        = string
  description = "Public domain for the web Ingress (e.g. engrams.example.com). Drives the ACM certificate + the values output; DNS validation + the final CNAME are `route53_zone_id` or manual records."

  validation {
    condition     = can(regex("^[a-z0-9.-]+\\.[a-z]{2,}$", var.domain))
    error_message = "domain must be a hostname like engrams.example.com (empty means $DOMAIN was unset)."
  }
}

variable "route53_zone_id" {
  type        = string
  description = "Optional: a Route53 hosted zone (this account) — ACM validation records are created automatically. Empty = create the validation records yourself (the `acm_validation_records` output)."
  default     = ""
}

variable "admin_email" {
  type        = string
  description = "Bootstrap admin: promoted to role 'admin' on first sign-in."

  validation {
    condition     = can(regex("^[^@\\s]+@[^@\\s]+$", var.admin_email))
    error_message = "admin_email must be an email address (empty means $ADMIN_EMAIL was unset)."
  }
}

variable "oauth_issuer" {
  type        = string
  description = "Issuer URL of the OIDC identity provider people sign in through (orchestrator.auth.oauth.issuer). Default: Google. A Cognito user pool is https://cognito-idp.<region>.amazonaws.com/<user-pool-id>."
  default     = "https://accounts.google.com"

  validation {
    condition     = can(regex("^https://", var.oauth_issuer))
    error_message = "oauth_issuer must be an https URL."
  }
}

variable "oauth_client_id" {
  type        = string
  description = "Client id of the OAuth client registered with the provider. Its redirect URI is https://<domain>/api/auth/oauth2/callback/sso. The client SECRET goes in the oauth-client-secret shell, never in Terraform."

  validation {
    condition     = length(trimspace(var.oauth_client_id)) > 0
    error_message = "oauth_client_id must not be empty (empty means $OAUTH_CLIENT_ID was unset)."
  }
}

variable "oauth_allowed_domains" {
  type        = list(string)
  description = "Who may sign in: every account of these email domains (for Google, the Workspace domain). An OAuth client admits every account the provider has, so this must name at least one domain; [\"*\"] admits them all."

  validation {
    condition     = length(var.oauth_allowed_domains) > 0 && alltrue([for d in var.oauth_allowed_domains : length(trimspace(d)) > 0])
    error_message = "oauth_allowed_domains must name at least one domain, for example [\"example.com\"]."
  }
}

variable "kvm_instance_type" {
  type        = string
  description = "Intel KVM-capable type (see the kvm-nodegroup module). Default is the nested-virt m8i shape nearest above GCP's c3-standard-22; set m7i.metal-24xl for CPUID parity with a GCP C3 fleet (metal quota, ~3× the cost)."
  default     = "m8i.8xlarge"
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
