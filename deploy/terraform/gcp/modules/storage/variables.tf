variable "bucket_name" {
  type        = string
  description = "Globally-unique bucket name. Convention: engram-chunks-<env>-<random6>."
}

variable "location" {
  type        = string
  description = "Bucket location. Regional unless you need multi-region read."
  default     = "US"
}

variable "force_destroy" {
  type        = bool
  description = "Allow `terraform destroy` to wipe non-empty buckets. Leave false in prod."
  default     = false
}

variable "user_sa_account_id" {
  type        = string
  description = "Account_id (no domain suffix) for the SA that reads + writes chunks."
  default     = "engram-chunks-user"
}

variable "labels" {
  type        = map(string)
  description = "Labels merged into bucket + child resources."
  default     = {}
}
