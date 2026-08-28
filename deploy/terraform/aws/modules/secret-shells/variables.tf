variable "name_prefix" {
  type        = string
  description = "Slash-namespace prefix (e.g. `engram` → `engram/auth-tokens`)."
}

variable "recovery_window_in_days" {
  type        = number
  description = "Secrets Manager deletion recovery window. 0 during bring-up (immediate delete); raise for prod."
  default     = 0
}

variable "tags" {
  type        = map(string)
  description = "Tags on the secrets."
  default     = {}
}
