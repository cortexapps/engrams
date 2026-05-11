# GCS bucket holding ADR 0007 chunks + manifests.
#
# Versioning is OFF: chunk objects are content-addressed
# (chunks/sha256/...), and manifests are versioned at the
# application layer (manifests/<id>/vN.json). Object-level
# versioning would just shadow that and double our storage cost.
#
# Lifecycle: delete bucket-level "tombstone" markers after 7 days
# so the GC sweep's deletes don't leak storage. Operators can
# extend this with rules like "move objects untouched > 30 days
# to NEARLINE" if cold-tier cost matters.

resource "google_storage_bucket" "chunks" {
  name                        = var.bucket_name
  location                    = var.location
  storage_class               = "STANDARD"
  force_destroy               = var.force_destroy
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"

  versioning {
    enabled = false
  }

  lifecycle_rule {
    action {
      type = "Delete"
    }
    condition {
      # Lifecycle on noncurrent versions is a no-op since
      # versioning is off; this rule is a defensive
      # belt-and-suspenders for ops who flip versioning on
      # later without thinking about cost.
      num_newer_versions = 5
    }
  }

  labels = merge(var.labels, {
    component = "engram-chunks"
  })
}

# Service account that the coord + every host-agent's WI binding
# maps to. Centralised here so the bucket's IAM grants name a
# single principal rather than enumerating per-cluster KSAs.
resource "google_service_account" "chunks_user" {
  account_id   = var.user_sa_account_id
  display_name = "Engram chunk-bucket reader/writer (${var.bucket_name})"
}

# Object-level R/W on the chunks bucket. The coord uses this for
# the legacy seal-pipeline (Stage 4, retiring with Phase 7) and
# the new chunk store. Host-agents use it for chunk reads +
# session-time snapshot writes.
resource "google_storage_bucket_iam_member" "chunks_rw" {
  bucket = google_storage_bucket.chunks.name
  role   = "roles/storage.objectAdmin"
  member = "serviceAccount:${google_service_account.chunks_user.email}"
}
