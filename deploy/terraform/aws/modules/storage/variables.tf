variable "bucket_name" {
  type        = string
  description = "Globally-unique bucket name. Convention: engram-durable-storage-<random6>."
}

variable "force_destroy" {
  type        = bool
  description = "Allow `terraform destroy` to wipe a non-empty bucket. Leave false in prod."
  default     = false
}

variable "tags" {
  type        = map(string)
  description = "Tags on the bucket."
  default     = {}
}
