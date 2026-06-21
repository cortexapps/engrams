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
