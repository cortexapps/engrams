//! ADR 0014 warm-pool template snapshots.
//!
//! A [`TemplateRecord`] is the persistent mapping from a session
//! tuple `(image_repo, image_tag, harness_pack_uri)` to the
//! portable [`SnapshotMetadata`] the image-builder produced at
//! bake time. The host-agent's warm-pool refill loop polls the
//! coord for active templates and restores N microVMs per row;
//! the coord scheduler resolves a session's spec to a candidate
//! `template_ref` before parallel-asking hosts for a warm slot.
//!
//! Lifecycle: a fresh row lands on each successful canonical
//! capture during a bake. The same `(repo, tag, harness)` triple
//! can have multiple rows over time as templates rebake; only one
//! row per triple has `active = true` at any moment. Hosts retain
//! 60 s of grace on inactive refs before draining stale warm
//! slots (see ADR 0014 §M1.6).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::{SnapshotId, TemplateRef};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TemplateRecord {
    pub template_ref: TemplateRef,
    /// Image registry tuple. ADR 0014 M1.12 (option D) made
    /// templates harness-agnostic: one bake per
    /// `(image_repo, image_tag)` regardless of harness.
    /// `harness_pack_uri` is now `Option<String>` carried for
    /// backwards-compat with pre-M1.12 rows; new rows write `None`.
    /// The unique key dropped this column too — see migration 0029.
    pub image_repo: String,
    pub image_tag: String,
    #[serde(default)]
    pub harness_pack_uri: Option<String>,
    /// Points at the `snapshots` row holding the full portable
    /// metadata (memory_manifest + state_blob_key + sidecar_blob_key
    /// + source_sandbox_id).
    pub snapshot_id: SnapshotId,
    /// Resource shape the warm slot is restored at. Sessions whose
    /// spec doesn't match are rejected from the warm-lease path
    /// (cold-create still works).
    pub vcpus: u32,
    pub memory_mib: u32,
    pub created_at: DateTime<Utc>,
    /// TRUE iff this is the latest bake for its
    /// `(image_repo, image_tag)` pair. Updated to FALSE atomically
    /// when a new template_ref lands for the same pair.
    pub active: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample() -> TemplateRecord {
        TemplateRecord {
            template_ref: TemplateRef::new(),
            image_repo: "cortexapps/engrams-internal/demo".into(),
            image_tag: "warm-734a0b5".into(),
            harness_pack_uri: Some("ghcr.io/cortexapps/engrams/harness-claude:b9dd2d1".into()),
            snapshot_id: SnapshotId::new(),
            vcpus: 4,
            memory_mib: 2048,
            created_at: Utc.with_ymd_and_hms(2026, 5, 15, 12, 0, 0).unwrap(),
            active: true,
        }
    }

    #[test]
    fn round_trips_via_serde_json() {
        let t = sample();
        let json = serde_json::to_string(&t).unwrap();
        let back: TemplateRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.template_ref, t.template_ref);
        assert_eq!(back.image_repo, t.image_repo);
        assert_eq!(back.snapshot_id, t.snapshot_id);
        assert_eq!(back.vcpus, t.vcpus);
        assert_eq!(back.active, t.active);
    }
}
