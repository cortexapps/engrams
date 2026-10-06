variable "project_id" {
  type        = string
  description = "GCP project id."
}

variable "region" {
  type        = string
  description = "Region for the cluster, subnet, and database."
}

variable "name_prefix" {
  type        = string
  description = "Prefix for every named resource."
  default     = "engram"
}

variable "domain" {
  type        = string
  description = "Public domain for the web Ingress (e.g. engrams.example.com). Drives the managed certificate + the values output; DNS itself is `dns_zone_name` or a manual A record to the static IP output."
}

variable "dns_zone_name" {
  type        = string
  description = "Optional: a Cloud DNS managed zone (in this project) to create the A record in. Empty = create the record yourself from the `web_static_ip` output."
  default     = ""
}

variable "admin_email" {
  type        = string
  description = "Bootstrap admin: promoted to role 'admin' on first sign-in (the values output wires it to orchestrator.auth.adminEmails)."
}

variable "oauth_issuer" {
  type        = string
  description = "Issuer URL of the OIDC identity provider people sign in through (orchestrator.auth.oauth.issuer). Default: Google."
  default     = "https://accounts.google.com"
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

variable "kvm_machine_type" {
  type        = string
  description = "Intel nested-virt machine type for the host pool (see the gke-kvm-pool module)."
  default     = "c3-standard-22"
}

variable "kvm_node_locations" {
  type        = list(string)
  description = "Zones for the KVM pool (production runs one zone). Null lets GKE spread."
  default     = null
}

variable "kvm_initial_node_count" {
  type        = number
  description = "Seed size for the KVM pool (the operator owns it afterwards)."
  default     = 2
}

variable "cloudsql_tier" {
  type        = string
  description = "Cloud SQL custom tier."
  default     = "db-custom-2-7680"
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
  description = "The coordinator's K8s ServiceAccount name (values-gcp example: engram-coordinator)."
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

variable "labels" {
  type        = map(string)
  description = "Labels applied to every labelable resource (cost attribution)."
  default     = {}
}
