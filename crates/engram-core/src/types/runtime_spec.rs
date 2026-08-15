//! ADR 0077 phase 3: `RuntimeSpec` — a session's boot inputs as one
//! persisted document.
//!
//! Written in the create transaction, refreshed at eviction finalize;
//! consumed by create-boot, resume, queued re-prepare, and evac. This
//! ends the re-derivation class: a queued session no longer loses its
//! dynamic skill mounts (the ADR 0055 TODO(P1-D) — the scanner booted
//! it with base skills only), the harness selection has one home, and
//! the egress template can't be "re-derived differently" post-resume
//! (the host substitutes its own guest_ip into the placeholder).
//!
//! serde-versioned: the JSONB carries `"v": 1` so a schema evolution is
//! a tagged migration, not a silent shape break.

use serde::{Deserialize, Serialize};

use super::egress::AppEndpoint;

/// The persisted boot-input document. Phase 3 carries the fields whose
/// re-derivation was lossy or bug-prone; the egress template + sealed
/// secret refs (documented in ADR 0077) extend this in phase 3b, at
/// which point the resume path stops re-resolving them too.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSpec {
    /// Schema tag. Bump on any incompatible field change.
    #[serde(default = "default_version")]
    pub v: u32,
    /// ADR 0055 profile-selected skill bundle names (NOT the resolved
    /// mounts — those re-resolve against the current fleet stamp ∪
    /// catalog at boot, so a re-baked bundle's fresh sha is picked up).
    /// This is the concrete TODO(P1-D) fix: the names survive to the
    /// queue re-prepare and resume.
    #[serde(default)]
    pub selected_skills: Vec<String>,
    /// ADR 0062 harness catalog key. `None` for a dev-VM session.
    #[serde(default)]
    pub selected_harness: Option<String>,
    /// Image manifest workdir the harness runs in.
    #[serde(default)]
    pub workdir: Option<String>,
    /// ADR 0118: the session's apps, as `(hostname, port)`.
    ///
    /// They live here for the same reason `selected_skills` does: they are
    /// minted once, before the session exists, and every later boot has to
    /// reproduce them exactly. A resume that re-derived them would mint fresh
    /// hostnames, and the guest's env — fixed at the first bind — would then
    /// point at addresses nothing serves.
    #[serde(default)]
    pub apps: Vec<AppEndpoint>,
}

fn default_version() -> u32 {
    1
}

impl RuntimeSpec {
    pub fn new(
        selected_skills: Vec<String>,
        selected_harness: Option<String>,
        workdir: Option<String>,
        apps: Vec<AppEndpoint>,
    ) -> Self {
        Self {
            v: 1,
            selected_skills,
            selected_harness,
            workdir,
            apps,
        }
    }
}
