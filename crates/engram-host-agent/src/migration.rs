//! ADR 0045 Phase C1: the live-teleport SOURCE side.
//!
//! `MigrationCapture` freezes a sandbox for a move: pause + NBD drain +
//! FC diff capture + LOCAL-sink re-chunk — no GCS traffic on the pause
//! path; every byte the destination needs is then reachable through
//! this host's NVMe chunk cache or the export's snapshot dir. The guest
//! STAYS PAUSED and the sandbox is fenced (no flush publishes, no
//! checkpoints — the export holds the capture lock) until `Commit`
//! (destroy) or `Abort` (un-pause in place, zero loss).
//!
//! The export registry is the auth + lifetime story: a single-use
//! unguessable `export_id` names the export; `MigrationFetch` serves
//! ONLY artifacts on the export's allowlist; and the dumb-host TTL rule
//! cleans up if the coordinator vanishes — after [`EXPORT_TTL`] with no
//! commit/abort, the source asks the coordinator "do I still own this
//! session?" and either un-pauses (yes) or destroys (no), staying
//! paused and retrying while the coordinator is unreachable.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use engram_chunk_store::manifest::ChunkHash;
use engram_core::SandboxId;

/// No commit/abort within this window triggers the coordinator
/// ownership check (see module docs).
pub const EXPORT_TTL: Duration = Duration::from_secs(120);

/// One frozen sandbox's transferable artifact set.
pub struct MigrationExport {
    pub export_id: String,
    pub sandbox_id: SandboxId,
    /// Holds state.bin + the (patched) sidecar manifest.json.
    pub snapshot_dir: PathBuf,
    /// Chunk hashes `MigrationFetch` may serve — the rebuilt memory
    /// chunks + the pending-tier disk chunks, all resident in the
    /// host-local NVMe cache.
    pub allowed_chunks: HashSet<ChunkHash>,
    /// Drained-but-not-uploaded disk chunks, kept for the abort
    /// re-queue (`ChunkedDiskBackend::requeue_pending`).
    pub disk_pending: Option<crate::disk_daemon::PendingDiskFlush>,
    pub created_at: Instant,
    /// The sandbox's capture lock, held for the export's lifetime —
    /// this IS the checkpoint fence (the periodic driver's
    /// `capture_in_flight` try_lock keeps skipping).
    pub capture_guard: tokio::sync::OwnedMutexGuard<()>,
}

/// Per-host registry of open exports. One per sandbox at most (the
/// session lease serializes coordinator-side; this is the host-side
/// backstop).
#[derive(Default)]
pub struct MigrationRegistry {
    by_sandbox: DashMap<SandboxId, MigrationExport>,
}

impl MigrationRegistry {
    pub fn insert(&self, export: MigrationExport) -> bool {
        match self.by_sandbox.entry(export.sandbox_id) {
            dashmap::mapref::entry::Entry::Occupied(_) => false,
            dashmap::mapref::entry::Entry::Vacant(v) => {
                v.insert(export);
                true
            }
        }
    }

    /// Validate an export_id against a sandbox's open export. Constant
    /// observable behavior for unknown sandbox vs wrong nonce.
    pub fn validate(&self, sandbox_id: SandboxId, export_id: &str) -> bool {
        self.by_sandbox
            .get(&sandbox_id)
            .map(|e| constant_time_str_eq(&e.export_id, export_id))
            .unwrap_or(false)
    }

    /// Look up by export_id alone (the fetch path carries no sandbox
    /// id). Linear over open exports — bounded by the per-host
    /// migration concurrency cap (1 in practice).
    pub fn find_by_export_id(
        &self,
        export_id: &str,
    ) -> Option<dashmap::mapref::multiple::RefMulti<'_, SandboxId, MigrationExport>> {
        self.by_sandbox
            .iter()
            .find(|e| constant_time_str_eq(&e.export_id, export_id))
    }

    /// Is an export currently open for this sandbox?
    pub fn validate_open(&self, sandbox_id: SandboxId) -> bool {
        self.by_sandbox.contains_key(&sandbox_id)
    }

    pub fn remove(&self, sandbox_id: SandboxId) -> Option<MigrationExport> {
        self.by_sandbox.remove(&sandbox_id).map(|(_, e)| e)
    }

    /// Exports older than [`EXPORT_TTL`] (the dumb-host sweep input).
    pub fn expired(&self) -> Vec<SandboxId> {
        self.by_sandbox
            .iter()
            .filter(|e| e.created_at.elapsed() > EXPORT_TTL)
            .map(|e| e.sandbox_id)
            .collect()
    }

    pub fn mint_export_id() -> String {
        // 32 random bytes (two v4 UUIDs, OS RNG), hex — unguessable,
        // single-use.
        let (a, b) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        a.as_bytes()
            .iter()
            .chain(b.as_bytes())
            .map(|x| format!("{x:02x}"))
            .collect()
    }
}

/// Constant-time string compare (export_id nonces).
fn constant_time_str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_id_is_single_use_per_sandbox_and_validated() {
        let reg = MigrationRegistry::default();
        let id = SandboxId::new();
        let (lock_a, guard) = {
            let m = std::sync::Arc::new(tokio::sync::Mutex::new(()));
            let g = m.clone().try_lock_owned().unwrap();
            (m, g)
        };
        let _ = lock_a;
        let eid = MigrationRegistry::mint_export_id();
        assert_eq!(eid.len(), 64);
        assert!(reg.insert(MigrationExport {
            export_id: eid.clone(),
            sandbox_id: id,
            snapshot_dir: "/tmp".into(),
            allowed_chunks: HashSet::new(),
            disk_pending: None,
            created_at: Instant::now(),
            capture_guard: guard,
        }));
        assert!(reg.validate(id, &eid));
        assert!(!reg.validate(id, "wrong"));
        assert!(!reg.validate(SandboxId::new(), &eid));
        assert!(reg.find_by_export_id(&eid).is_some());

        // Second export on the same sandbox refused while one is open.
        let m2 = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        let g2 = m2.clone().try_lock_owned().unwrap();
        assert!(!reg.insert(MigrationExport {
            export_id: MigrationRegistry::mint_export_id(),
            sandbox_id: id,
            snapshot_dir: "/tmp".into(),
            allowed_chunks: HashSet::new(),
            disk_pending: None,
            created_at: Instant::now(),
            capture_guard: g2,
        }));

        assert!(reg.remove(id).is_some());
        assert!(
            !reg.validate(id, &eid),
            "removed export no longer validates"
        );
    }
}
