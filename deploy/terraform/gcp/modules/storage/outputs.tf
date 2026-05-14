output "bucket_name" {
  description = "Bucket name; what `ENGRAM_GCS_BUCKET` env gets set to on coord + host-agent."
  value       = google_storage_bucket.chunks.name
}

output "bucket_url" {
  description = "gs:// URL form of the bucket — handy for shell scripts."
  value       = google_storage_bucket.chunks.url
}
