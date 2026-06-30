//! ADR 0055 P2: the org-shared user-uploaded skill catalog.
//!
//! P1 leaned on the fleet's baked `current_bundles` stamp as the catalog of
//! admin skills. User uploads can't be baked into the host image, so P2 adds a
//! durable coordinator-side catalog (`mount_catalog` table) of content-addressed
//! skill bundles. The resolver reads **fleet stamp ∪ this catalog**; the catalog
//! shas fold into the bundle pin set so every host auto-stages an uploaded skill
//! via the existing materialize-by-sha path.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One live row of the `mount_catalog` table — a registered, content-addressed
/// user-uploaded skill bundle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSkill {
    /// Stable catalog id (uuid string).
    pub id: String,
    /// Uploader principal (the orchestrator's better-auth `user.id`).
    /// Attribution + GC ownership + quota — **not** an access boundary: the
    /// catalog is org-shared (ADR 0055 P2), so any profile may select any skill.
    pub owner: String,
    /// Logical bundle name, used verbatim in `profile.skills` and on the
    /// `CreateSessionRequest.selected_skills` wire. UNIQUE org-wide and forbidden
    /// from colliding with a fleet bundle name (enforced at registration).
    pub name: String,
    /// Human description for the catalog UI.
    pub description: String,
    /// Content address of the packed squashfs (its BlobStorage key is
    /// `bundles/sha256/<sha256>`; `AuxRoDrive::blob_key`).
    pub sha256: String,
    /// The `mount.json` baked into the squashfs root (stored for the record /
    /// debugging; the hot path never re-parses it — the manifest already lives
    /// inside the mounted bundle, read by `activate()`).
    pub mount_json: String,
    /// Packed squashfs size in bytes.
    pub size_bytes: i64,
    pub created_at: DateTime<Utc>,
}

/// ADR 0062: one live row of the `harness_catalog` table — a registered,
/// OCI-sourced agent harness. Built-in and custom harnesses are uniform here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogHarness {
    /// Stable catalog id (uuid string).
    pub id: String,
    /// Registering principal. Attribution / GC ownership / quota — **not** an
    /// access boundary (the catalog is org-shared, like skills).
    pub owner: String,
    /// Logical harness name, used verbatim on the `CreateSessionRequest.harness`
    /// wire and as the catalog-squashfs subtree (`<name>/`). UNIQUE among live
    /// rows.
    pub name: String,
    /// The source OCI artifact reference the harness was registered from.
    pub oci_ref: String,
    /// The OCI manifest digest resolved at pull time (audit trail; the pulled
    /// bytes are content-addressed by [`Self::tree_sha256`]).
    pub manifest_digest: String,
    /// The harness's `harness.toml` content (ADR 0063). Parsed into a
    /// [`HarnessDescriptor`](crate::types::harness::HarnessDescriptor) for the
    /// orchestrator-facing projection + the coordinator's launch contract.
    pub descriptor_toml: String,
    /// Content address of the harness's extracted-tree tarball (its BlobStorage
    /// key is `harness-trees/sha256/<tree_sha256>`), materialized to re-pack the
    /// catalog squashfs on any registration change.
    pub tree_sha256: String,
    /// Size of the extracted-tree tarball in bytes.
    pub tree_size_bytes: i64,
    pub created_at: DateTime<Utc>,
}

impl CatalogHarness {
    /// Parse the stored `harness.toml` into a typed descriptor.
    pub fn descriptor(&self) -> Result<crate::types::harness::HarnessDescriptor, String> {
        crate::types::harness::HarnessDescriptor::parse(&self.descriptor_toml)
    }
}

/// The fields needed to register (upsert) a harness — bundled so
/// `MetadataStore::register_harness` keeps a lean signature.
#[derive(Clone, Copy, Debug)]
pub struct HarnessRegistration<'a> {
    /// Attribution / GC ownership (org-shared catalog).
    pub owner: &'a str,
    /// Logical harness name (UNIQUE among live rows).
    pub name: &'a str,
    /// Source OCI artifact reference.
    pub oci_ref: &'a str,
    /// OCI manifest digest resolved at pull time.
    pub manifest_digest: &'a str,
    /// The harness's `harness.toml` content (ADR 0063).
    pub descriptor_toml: &'a str,
    /// Content address of the extracted-tree tarball blob.
    pub tree_sha256: &'a str,
    /// Size of that tarball in bytes.
    pub tree_size_bytes: i64,
}
