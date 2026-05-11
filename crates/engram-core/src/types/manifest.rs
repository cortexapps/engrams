//! ADR 0007 chunk-manifest reference type.
//!
//! Kept in `engram-core` rather than `engram-chunk-store` so the
//! shared trait types (`SandboxSpec`, `SnapshotMetadata`, etc.) can
//! reference it without engram-core taking a dep on engram-chunk-
//! store (which itself depends on engram-core for `BlobStorage`).
//!
//! The chunk store re-exports `ManifestRef` from this module so
//! callers see a single canonical name.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable, versioned identifier for a chunked manifest. The
/// `manifest_id` is permanent (every snapshot of session X uses the
/// same id); the `version` ticks monotonically as the manifest
/// mutates.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ManifestRef {
    pub manifest_id: Uuid,
    pub version: u64,
}

impl ManifestRef {
    /// Build a fresh ref at version 1 with a new random id.
    pub fn new() -> Self {
        Self {
            manifest_id: Uuid::new_v4(),
            version: 1,
        }
    }

    /// Tick to the next version of the same manifest_id.
    #[must_use]
    pub fn next_version(self) -> Self {
        Self {
            manifest_id: self.manifest_id,
            version: self.version + 1,
        }
    }

    /// Object-storage key under `manifests/` for this version.
    pub fn storage_key(&self) -> String {
        format!("manifests/{}/v{}.json", self.manifest_id, self.version)
    }

    /// Path prefix common to all versions of this manifest. Useful
    /// for listing all versions of one id.
    pub fn id_prefix(manifest_id: Uuid) -> String {
        format!("manifests/{manifest_id}/")
    }
}

impl Default for ManifestRef {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ManifestRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@v{}", self.manifest_id, self.version)
    }
}
