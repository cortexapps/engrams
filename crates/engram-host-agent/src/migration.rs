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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use engram_chunk_store::manifest::ChunkHash;
use engram_core::SandboxId;

/// No commit/abort within this window triggers the coordinator
/// ownership check (see module docs). For post-copy exports the clock
/// runs from the LAST page-serving activity, not creation — an
/// actively-draining export is alive by definition; 120 s of silence
/// is the trigger.
pub const EXPORT_TTL: Duration = Duration::from_secs(120);

/// ADR 0045 C2: a sandbox's role in an in-flight post-copy migration.
/// Both roles fence the normal lifecycle drivers: the SOURCE is a
/// frozen page server (never idle-evicted, never checkpointed, NEVER
/// self-resumed once state has shipped); the DEST is live but its
/// durability machinery stays gated until the drain + catch-up land.
/// Persisted into the FC sandbox manifest so a host-agent restart's
/// reattach pass (ADR 0044 K2) re-learns it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationRole {
    PostCopySource,
    PostCopyDest,
}

impl MigrationRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PostCopySource => "post-copy-source",
            Self::PostCopyDest => "post-copy-dest",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "post-copy-source" => Some(Self::PostCopySource),
            "post-copy-dest" => Some(Self::PostCopyDest),
            _ => None,
        }
    }
}

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
    /// ADR 0045 C2: this export serves a post-copy move (the guest
    /// already resumed on the dest; this frozen source is a page
    /// server). Changes the TTL clock (last_activity, not created_at)
    /// and FORBIDS the abort-unpause arm once `state_served` is set.
    pub post_copy: bool,
    /// ADR 0045 C2 split-brain guard: set the moment `state.bin`
    /// leaves this host (`MigrationFetch` StateBin). From then on the
    /// dest may be running this state — the source must NEVER
    /// self-resume, even if the coordinator says we still own the
    /// session (`ttl_verdict` returns StayPaused, converging via the
    /// scanner within one cycle).
    pub state_served: Arc<AtomicBool>,
    /// Last page/artifact-serving activity (the post-copy TTL clock).
    pub last_activity: Arc<std::sync::Mutex<Instant>>,
    /// The sandbox's capture lock, held for the export's lifetime —
    /// this IS the checkpoint fence (the periodic driver's
    /// `capture_in_flight` try_lock keeps skipping).
    pub capture_guard: tokio::sync::OwnedMutexGuard<()>,
}

impl MigrationExport {
    /// Refresh the activity clock (every artifact/page serve).
    pub fn touch(&self) {
        *self.last_activity.lock().expect("last_activity poisoned") = Instant::now();
    }
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

    /// The open export's id for a sandbox (the TTL sweep's handle).
    pub fn export_id_of(&self, sandbox_id: SandboxId) -> Option<String> {
        self.by_sandbox
            .get(&sandbox_id)
            .map(|e| e.export_id.clone())
    }

    /// Exports older than [`EXPORT_TTL`] (the dumb-host sweep input).
    /// Post-copy exports age from their last serving activity (an
    /// actively-draining export is alive); C1 exports from creation.
    pub fn expired(&self) -> Vec<SandboxId> {
        self.by_sandbox
            .iter()
            .filter(|e| {
                let anchor = if e.post_copy {
                    *e.last_activity.lock().expect("last_activity poisoned")
                } else {
                    e.created_at
                };
                anchor.elapsed() > EXPORT_TTL
            })
            .map(|e| e.sandbox_id)
            .collect()
    }

    /// The split-brain flag for a sandbox's open export (TTL sweep
    /// input). `false` when no export is open.
    pub fn state_served(&self, sandbox_id: SandboxId) -> bool {
        self.by_sandbox
            .get(&sandbox_id)
            .map(|e| e.state_served.load(Ordering::SeqCst))
            .unwrap_or(false)
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

/// ADR 0045 C1: what the TTL sweep should do with an expired export,
/// given the coordinator's ownership answer. Pure — the decision the
/// dumb-host rule encodes, unit-tested apart from any I/O.
#[derive(Debug, PartialEq, Eq)]
pub enum TtlVerdict {
    /// The coordinator still binds this sandbox to the session: the
    /// move never landed. Un-pause in place (abort) — zero loss.
    AbortInPlace,
    /// Ownership moved on (scanner rehome, or rebind without the
    /// commit). The frozen source is STALE — resuming it would split
    /// state; destroy it.
    Destroy,
    /// Coordinator unreachable: stay paused and retry next sweep —
    /// never guess about ownership.
    StayPaused,
}

/// `ownership = None` ⇒ unreachable; `session_bound = false` ⇒ the
/// export's sandbox has no session binding (anonymous — nothing can
/// reclaim it, destroy).
///
/// ADR 0045 C2: `state_served` FORBIDS the yes⇒un-pause arm. Once
/// state.bin has shipped, the dest may be running this state — an
/// un-pause would be the split-brain. Stay paused instead; if the
/// rebind never landed the session sits Evacuating, the scanner (lease
/// expired) rehomes it from the durable row, the ownership answer
/// flips to `false`, and the NEXT sweep destroys this corpse — paused
/// ≤ one scanner cycle, never a zombie.
pub fn ttl_verdict(session_bound: bool, ownership: Option<bool>, state_served: bool) -> TtlVerdict {
    if !session_bound {
        return TtlVerdict::Destroy;
    }
    match ownership {
        Some(true) if state_served => TtlVerdict::StayPaused,
        Some(true) => TtlVerdict::AbortInPlace,
        Some(false) => TtlVerdict::Destroy,
        None => TtlVerdict::StayPaused,
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
    fn ttl_verdict_encodes_the_dumb_host_rule() {
        assert_eq!(
            ttl_verdict(true, Some(true), false),
            TtlVerdict::AbortInPlace
        );
        assert_eq!(ttl_verdict(true, Some(false), false), TtlVerdict::Destroy);
        assert_eq!(ttl_verdict(true, None, false), TtlVerdict::StayPaused);
        assert_eq!(ttl_verdict(false, Some(true), false), TtlVerdict::Destroy);
        assert_eq!(ttl_verdict(false, None, false), TtlVerdict::Destroy);
    }

    /// ADR 0045 C2: once state.bin shipped, yes⇒un-pause is FORBIDDEN
    /// (split-brain); everything else is unchanged.
    #[test]
    fn ttl_verdict_state_served_forbids_unpause() {
        assert_eq!(ttl_verdict(true, Some(true), true), TtlVerdict::StayPaused);
        assert_eq!(ttl_verdict(true, Some(false), true), TtlVerdict::Destroy);
        assert_eq!(ttl_verdict(true, None, true), TtlVerdict::StayPaused);
        assert_eq!(ttl_verdict(false, Some(true), true), TtlVerdict::Destroy);
    }

    #[test]
    fn migration_role_round_trips_persistence_strings() {
        for role in [MigrationRole::PostCopySource, MigrationRole::PostCopyDest] {
            assert_eq!(MigrationRole::parse(role.as_str()), Some(role));
        }
        assert_eq!(MigrationRole::parse("garbage"), None);
    }

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
            post_copy: false,
            state_served: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_activity: std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now())),
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
            post_copy: false,
            state_served: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_activity: std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now())),
            capture_guard: g2,
        }));

        assert!(reg.remove(id).is_some());
        assert!(
            !reg.validate(id, &eid),
            "removed export no longer validates"
        );
    }
}
