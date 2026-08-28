# S3 bucket holding ADR 0007 chunks + manifests — the AWS twin of
# gcp/modules/storage.
#
# Versioning is OFF: chunk objects are content-addressed
# (chunks/sha256/...), and manifests are versioned at the application
# layer (manifests/<id>/vN.json). Object versioning would shadow that
# and double storage cost.
#
# The abort-incomplete-multipart rule pairs with the S3 blob
# backend's streaming design (ADR 0122): `put_streaming` best-effort
# aborts a failed multipart upload, but a crashed process can't —
# without this rule its parts would linger invisibly and bill
# forever.

resource "aws_s3_bucket" "chunks" {
  bucket        = var.bucket_name
  force_destroy = var.force_destroy

  tags = var.tags
}

resource "aws_s3_bucket_public_access_block" "chunks" {
  bucket = aws_s3_bucket.chunks.id

  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_server_side_encryption_configuration" "chunks" {
  bucket = aws_s3_bucket.chunks.id

  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

resource "aws_s3_bucket_lifecycle_configuration" "chunks" {
  bucket = aws_s3_bucket.chunks.id

  rule {
    id     = "abort-incomplete-multipart"
    status = "Enabled"

    filter {}

    abort_incomplete_multipart_upload {
      days_after_initiation = 7
    }
  }
}
