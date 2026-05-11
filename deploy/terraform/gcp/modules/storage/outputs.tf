output "bucket_name" {
  description = "Bucket name; what `ENGRAM_GCS_BUCKET` env gets set to on coord + host-agent."
  value       = google_storage_bucket.chunks.name
}

output "bucket_url" {
  description = "gs:// URL form of the bucket — handy for shell scripts."
  value       = google_storage_bucket.chunks.url
}

output "chunks_user_email" {
  description = "GSA email — what the coord's KSA + host-agent's instance SA need to bind via WI."
  value       = google_service_account.chunks_user.email
}

output "chunks_user_id" {
  description = "Fully-qualified resource id of the chunks user SA."
  value       = google_service_account.chunks_user.id
}
