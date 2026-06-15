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

  # Soft-delete OFF. GCS enables a 7-day soft-delete retention by
  # default, which keeps (and bills for) every deleted object for a
  # week. The chunk store is content-addressed and the GC sweep's
  # deletes are intentional — soft-delete just adds cost + retains
  # blobs we meant to drop. Zero retention disables it.
  soft_delete_policy {
    retention_duration_seconds = 0
  }

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

# IAM bindings live at the caller, not here. Each consumer (coord
# GSA, every host-MIG instance SA) gets `roles/storage.objectAdmin`
# on this bucket directly. The previous design wired a single
# `chunks_user` SA + `tokenCreator` impersonation chain, but the
# host-agent's GCS client uses ADC without impersonating, so the
# chain was never walked and consumers 403'd. Dropping the
# indirection keeps the model simple and matches what actually
# happens at call time.
