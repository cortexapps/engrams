output "bucket_name" {
  value       = aws_s3_bucket.chunks.bucket
  description = "Bucket name; what `ENGRAM_S3_BUCKET` gets set to on coord + host-agent."
}

output "bucket_arn" {
  value       = aws_s3_bucket.chunks.arn
  description = "For IAM policy resources."
}
