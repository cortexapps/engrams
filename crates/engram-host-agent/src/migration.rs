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
use std::time::Duration;

use dashmap::DashMap;
use engram_chunk_store::manifest::ChunkHash;
use engram_core::SandboxId;

/// No commit/abort within this window triggers the coordinator
/// ownership check (see module docs). The clock runs from the LAST
/// artifact/page-serving activity, not creation — for BOTH C1 (gRPC
/// `migration_fetch`) and post-copy (the TCP page server) exports: an
/// export actively serving the move is alive by definition; 120 s of
/// silence is the trigger.
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
    /// ADR 0045 C2 disk post-copy: the sealed dirty/pending disk
    /// tiers, raw bytes in RAM — served by index over
    /// `MigrationFetch::DiskChunkAt`, re-queued into `dirty` on abort,
    /// dropped on commit (the dest drained them).
    pub disk_seal: Option<Arc<crate::disk_daemon::PostCopyDiskSeal>>,
    /// ADR 0098 P8: the TTL clock is the INJECTED monotonic clock
    /// (`now_mono`), not a raw `Instant` — expiry DECIDES destroy/abort,
    /// so it is decision-feeding time (D1), and the paused sim clock
    /// drives it deterministically.
    pub clock: Arc<dyn engram_core::traits::Clock>,
    pub created_at: Duration,
    /// ADR 0045 C2: this export serves a post-copy move (the guest
    /// already resumed on the dest; this frozen source is a page
    /// server). FORBIDS the abort-unpause arm once `state_served` is
    /// set. (The TTL clock is `last_activity` for ALL exports now — see
    /// `expired()` — so this no longer gates the anchor.)
    pub post_copy: bool,
    /// ADR 0045 C2 split-brain guard: set the moment `state.bin`
    /// leaves this host (`MigrationFetch` StateBin). From then on the
    /// dest may be running this state — the source must NEVER
    /// self-resume, even if the coordinator says we still own the
    /// session (`ttl_verdict` returns StayPaused, converging via the
    /// scanner within one cycle).
    pub state_served: Arc<AtomicBool>,
    /// Last page/artifact-serving activity (the export TTL clock), as a
    /// `now_mono` reading.
    pub last_activity: Arc<std::sync::Mutex<Duration>>,
    /// The sandbox's capture lock, held for the export's lifetime —
    /// this IS the checkpoint fence (the periodic driver's
    /// `capture_in_flight` try_lock keeps skipping).
    pub capture_guard: tokio::sync::OwnedMutexGuard<()>,
}

impl MigrationExport {
    /// Refresh the activity clock (every artifact/page serve) off the
    /// injected monotonic clock.
    pub fn touch(&self) {
        *self.last_activity.lock().expect("last_activity poisoned") = self.clock.now_mono();
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

    /// Atomically validate `export_id` against the sandbox's open export
    /// AND remove it, returning the export iff the nonce matched. This
    /// is the ONLY safe way to consume an export from a commit/abort:
    /// the separate `validate()` + `remove()` is a TOCTOU window where
    /// two concurrent callers (a coordinator RPC and the dumb-host TTL
    /// sweep) both pass `validate` and then the loser's `remove` returns
    /// `None`. DashMap's `remove_if` holds the shard lock across the
    /// predicate + removal, so exactly one racer wins; the other sees
    /// `None` and the caller maps it to a clean `NotFound`. Constant-time
    /// nonce compare, same as `validate`.
    pub fn remove_validated(
        &self,
        sandbox_id: SandboxId,
        export_id: &str,
    ) -> Option<MigrationExport> {
        self.by_sandbox
            .remove_if(&sandbox_id, |_, e| {
                constant_time_str_eq(&e.export_id, export_id)
            })
            .map(|(_, e)| e)
    }

    /// The open export's id for a sandbox (the TTL sweep's handle).
    pub fn export_id_of(&self, sandbox_id: SandboxId) -> Option<String> {
        self.by_sandbox
            .get(&sandbox_id)
            .map(|e| e.export_id.clone())
    }

    /// Exports older than [`EXPORT_TTL`] (the dumb-host sweep input).
    /// ALL exports age from their last serving activity, not creation:
    /// an export actively serving fetches/pages is alive by definition.
    /// `touch()` is called on every serve for BOTH C1 (gRPC
    /// `migration_fetch`) and post-copy (the peer page server) exports;
    /// a fresh export's `last_activity` is seeded to its creation
    /// instant, so an export that has served NOTHING still ages from
    /// creation. (Previously C1 anchored on `created_at`, so a >120 s
    /// live-teleport that was actively serving `migration_fetch`
    /// streams was spuriously aborted mid-read — issue #216 Gap 1.)
    pub fn expired(&self) -> Vec<SandboxId> {
        self.by_sandbox
            .iter()
            .filter(|e| {
                let anchor = *e.last_activity.lock().expect("last_activity poisoned");
                e.clock.now_mono().saturating_sub(anchor) > EXPORT_TTL
            })
            .map(|e| e.sandbox_id)
            .collect()
    }

    /// Refresh a sandbox's open export activity anchor (a serve landed).
    /// `false` when no export is open.
    pub fn touch(&self, sandbox_id: SandboxId) -> bool {
        self.by_sandbox
            .get(&sandbox_id)
            .map(|e| {
                e.touch();
                true
            })
            .unwrap_or(false)
    }

    /// Raise a sandbox's open export split-brain flag (`state.bin` left
    /// the host) and refresh its activity anchor. `false` when no export
    /// is open.
    pub fn mark_state_served(&self, sandbox_id: SandboxId) -> bool {
        self.by_sandbox
            .get(&sandbox_id)
            .map(|e| {
                e.state_served.store(true, Ordering::SeqCst);
                e.touch();
                true
            })
            .unwrap_or(false)
    }

    /// The split-brain flag for a sandbox's open export (TTL sweep
    /// input). `false` when no export is open.
    pub fn state_served(&self, sandbox_id: SandboxId) -> bool {
        self.by_sandbox
            .get(&sandbox_id)
            .map(|e| e.state_served.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    // ADR 0098 D1 carve-out, reaffirmed P8: this mints an UNGUESSABLE single-use security
    // token (the migration export id / peer token), not a simulation-visible
    // identifier — the same rationale D1 uses to keep crypto key material on
    // `OsRng` rather than the seeded `entropy`. Seeding it would make the
    // token predictable, which is the opposite of the requirement.
    #[allow(clippy::disallowed_methods)]
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

/// Issue #216 Gap 3: what a REATTACHED frozen post-copy source should
/// do on one ownership tick, given (a) whether its session binding has
/// rehydrated into the in-memory map yet and (b) the coordinator's
/// ownership answer once it has.
///
/// A frozen post-copy source NEVER self-resumes (state.bin already
/// shipped to the dest), so the only outcomes are `Destroy` (ownership
/// has explicitly moved on) or `StayPaused` (everything else). Unlike
/// `ttl_verdict`, a MISSING in-memory binding is NOT "no session owns
/// this" — it is "we have not LEARNED the binding yet": the register
/// task's `rehydrate_survivors` races this 30 s tick and backs off up to
/// 30 s when the coordinator is unreachable (the very condition that
/// triggered the restart). Destroying on that transient `None` killed a
/// healthy source mid-move. The invariant ("never guess about
/// ownership; unreachable ⇒ stay paused") demands we ask the coordinator
/// — and only an explicit `owned == false` destroys.
#[derive(Debug, PartialEq, Eq)]
pub enum ReattachSourceVerdict {
    /// Ownership explicitly moved on (`owned == false`): the frozen
    /// source is stale — destroy it.
    Destroy,
    /// Binding not yet rehydrated, coordinator unreachable, or
    /// coordinator still owns the session: stay paused, retry next tick.
    StayPaused,
}

/// `session_bound = false` ⇒ the in-memory binding has not rehydrated
/// yet (retry; NEVER destroy on this alone). `ownership` is only
/// meaningful once bound: `Some(false)` ⇒ destroy; `Some(true)` /
/// `None` (unreachable) ⇒ stay paused.
pub fn reattach_source_verdict(
    session_bound: bool,
    ownership: Option<bool>,
) -> ReattachSourceVerdict {
    if !session_bound {
        return ReattachSourceVerdict::StayPaused;
    }
    match ownership {
        Some(false) => ReattachSourceVerdict::Destroy,
        Some(true) | None => ReattachSourceVerdict::StayPaused,
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
    // tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]
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
    ///
    /// Issue #216 Gap 1: `state_served` now arms for C1 too
    /// (`migration_fetch` sets it on any StateBin serve, dropping the
    /// old `post_copy` gate). `ttl_verdict` is mode-agnostic, so the
    /// SAME forbidden-unpause arm protects a C1 export whose state.bin
    /// shipped before it expired — a >120 s C1 teleport that already
    /// shipped state must STAY PAUSED, never `AbortInPlace` (which would
    /// resume the source while the dest may be running that state).
    #[test]
    fn ttl_verdict_state_served_forbids_unpause() {
        // C2 (post-copy) — the original cases.
        assert_eq!(ttl_verdict(true, Some(true), true), TtlVerdict::StayPaused);
        assert_eq!(ttl_verdict(true, Some(false), true), TtlVerdict::Destroy);
        assert_eq!(ttl_verdict(true, None, true), TtlVerdict::StayPaused);
        assert_eq!(ttl_verdict(false, Some(true), true), TtlVerdict::Destroy);

        // C1 with state_served armed: identical verdicts. The crucial
        // arm is `(bound, owned=true, state_served=true)` ⇒ StayPaused,
        // NOT AbortInPlace — the C1 split-brain protection issue #216
        // wires by dropping the `post_copy` gate on the
        // `state_served`-set in `migration_fetch`.
        assert_eq!(
            ttl_verdict(true, Some(true), true),
            TtlVerdict::StayPaused,
            "C1 export that shipped state must not be un-paused (issue #216 Gap 1)"
        );
    }

    /// Issue #216 Gap 1: a C1 (non-post-copy) export that is actively
    /// serving fetches must NOT expire from `created_at` — `expired()`
    /// anchors on `last_activity`, and `migration_fetch` calls `touch()`
    /// for C1 too. We simulate by seeding `created_at` and the shared
    /// `last_activity` clock far in the past, then `touch()`ing: a C1
    /// export born >TTL ago but touched now must NOT be expired.
    /// A settable-mono test clock: `SystemClock::now_mono` anchors at
    /// construction, so a fresh test process cannot mint a "stale" mark by
    /// subtraction (it saturates to zero). This fake advances explicitly.
    #[derive(Debug)]
    struct TestMonoClock(std::sync::Mutex<Duration>);

    impl engram_core::traits::Clock for TestMonoClock {
        fn now_utc(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::DateTime::UNIX_EPOCH
        }
        fn now_mono(&self) -> Duration {
            *self.0.lock().unwrap()
        }
        fn sleep(
            &self,
            dur: Duration,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
            Box::pin(tokio::time::sleep(dur))
        }
    }

    #[test]
    fn c1_export_ages_from_last_activity_not_creation() {
        let reg = MigrationRegistry::default();
        let id = SandboxId::new();
        let eid = MigrationRegistry::mint_export_id();
        // The clock sits well past the TTL so a stale anchor is mintable.
        let clock: Arc<dyn engram_core::traits::Clock> =
            Arc::new(TestMonoClock(std::sync::Mutex::new(EXPORT_TTL * 3)));
        let stale = clock
            .now_mono()
            .saturating_sub(EXPORT_TTL + Duration::from_secs(60));
        let last_activity = Arc::new(std::sync::Mutex::new(stale));
        let guard = std::sync::Arc::new(tokio::sync::Mutex::new(()))
            .try_lock_owned()
            .unwrap();
        assert!(reg.insert(MigrationExport {
            export_id: eid,
            sandbox_id: id,
            snapshot_dir: "/tmp".into(),
            allowed_chunks: HashSet::new(),
            disk_pending: None,
            disk_seal: None,
            clock: clock.clone(),
            created_at: stale,
            // The bug specifically affected C1 (non-post-copy) exports.
            post_copy: false,
            state_served: Arc::new(AtomicBool::new(false)),
            last_activity: last_activity.clone(),
            capture_guard: guard,
        }));

        // Born >TTL ago AND silent >TTL ⇒ expired (the abandoned case
        // the sweep is for).
        assert_eq!(reg.expired(), vec![id], "stale C1 export must expire");

        // An active fetch refreshes the clock — the export is alive
        // again even though `created_at` is ancient. (Pre-fix: C1 aged
        // on `created_at`, so this stayed expired → spurious mid-read
        // abort of a healthy >120 s teleport.)
        *last_activity.lock().unwrap() = clock.now_mono();
        assert!(
            reg.expired().is_empty(),
            "a freshly-touched C1 export must NOT expire (issue #216 Gap 1)"
        );
    }

    /// Issue #216 Gap 3: a reattached frozen post-copy source must stay
    /// paused on a not-yet-rehydrated binding or an unreachable
    /// coordinator, and destroy ONLY on an explicit `owned == false`.
    #[test]
    fn reattach_source_verdict_destroys_only_on_explicit_unowned() {
        use ReattachSourceVerdict::*;
        // No binding learned yet ⇒ stay paused regardless of the
        // (irrelevant, un-asked) ownership input.
        assert_eq!(reattach_source_verdict(false, None), StayPaused);
        assert_eq!(reattach_source_verdict(false, Some(false)), StayPaused);
        assert_eq!(reattach_source_verdict(false, Some(true)), StayPaused);
        // Bound + coordinator unreachable ⇒ never guess; stay paused.
        assert_eq!(reattach_source_verdict(true, None), StayPaused);
        // Bound + still owned ⇒ stay paused (never self-resume).
        assert_eq!(reattach_source_verdict(true, Some(true)), StayPaused);
        // Bound + explicitly unowned ⇒ the ONLY destroy arm.
        assert_eq!(reattach_source_verdict(true, Some(false)), Destroy);
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
            disk_seal: None,
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            created_at: Duration::ZERO,
            post_copy: false,
            state_served: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_activity: std::sync::Arc::new(std::sync::Mutex::new(
                engram_core::traits::Clock::now_mono(&engram_core::traits::SystemClock::new()),
            )),
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
            disk_seal: None,
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            created_at: Duration::ZERO,
            post_copy: false,
            state_served: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_activity: std::sync::Arc::new(std::sync::Mutex::new(
                engram_core::traits::Clock::now_mono(&engram_core::traits::SystemClock::new()),
            )),
            capture_guard: g2,
        }));

        assert!(reg.remove(id).is_some());
        assert!(
            !reg.validate(id, &eid),
            "removed export no longer validates"
        );
    }

    fn dummy_export(sandbox_id: SandboxId, export_id: String) -> MigrationExport {
        // `try_lock_owned` consumes the Arc; the returned guard keeps its
        // own Arc to the Mutex, so it stays valid for the export's life.
        let guard = std::sync::Arc::new(tokio::sync::Mutex::new(()))
            .try_lock_owned()
            .unwrap();
        MigrationExport {
            export_id,
            sandbox_id,
            snapshot_dir: "/tmp".into(),
            allowed_chunks: HashSet::new(),
            disk_pending: None,
            disk_seal: None,
            clock: Arc::new(engram_core::traits::SystemClock::new()),
            created_at: Duration::ZERO,
            post_copy: false,
            state_served: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_activity: std::sync::Arc::new(std::sync::Mutex::new(
                engram_core::traits::Clock::now_mono(&engram_core::traits::SystemClock::new()),
            )),
            capture_guard: guard,
        }
    }

    /// `remove_validated` is the atomic consume that closes the
    /// validate→remove TOCTOU. It returns the export iff the nonce
    /// matches, and is idempotent-safe: a second consume (the race
    /// loser) gets `None`, never a panic.
    #[test]
    fn remove_validated_consumes_exactly_once_on_match() {
        let reg = MigrationRegistry::default();
        let id = SandboxId::new();
        let eid = MigrationRegistry::mint_export_id();
        assert!(reg.insert(dummy_export(id, eid.clone())));

        // Wrong nonce leaves the export in place (constant-time mismatch).
        assert!(reg.remove_validated(id, "wrong").is_none());
        assert!(reg.validate_open(id), "wrong-nonce consume is a no-op");

        // Correct nonce consumes it once...
        assert!(reg.remove_validated(id, &eid).is_some());
        // ...and the second consume (the race loser) gets None, no panic.
        assert!(reg.remove_validated(id, &eid).is_none());
        assert!(!reg.validate_open(id));
    }

    /// Regression for the TOCTOU panic (issue #203): two concurrent
    /// consumers of the SAME export — the coordinator's commit/abort RPC
    /// and the dumb-host TTL sweep — must yield exactly one winner and
    /// NEVER panic. With the old `validate()` + `remove().expect(...)`
    /// pattern both racers passed `validate` and the loser's `remove`
    /// returned `None`, panicking `.expect("validated above")` and (if it
    /// was the sweep task) permanently killing the TTL safety net.
    #[test]
    fn concurrent_remove_validated_has_exactly_one_winner_no_panic() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::Barrier;

        // Loop the race many times to make the interleaving likely.
        for _ in 0..2_000 {
            let reg = Arc::new(MigrationRegistry::default());
            let id = SandboxId::new();
            let eid = MigrationRegistry::mint_export_id();
            assert!(reg.insert(dummy_export(id, eid.clone())));

            let winners = Arc::new(AtomicUsize::new(0));
            let barrier = Arc::new(Barrier::new(2));
            let mut handles = Vec::new();
            for _ in 0..2 {
                let reg = reg.clone();
                let eid = eid.clone();
                let winners = winners.clone();
                let barrier = barrier.clone();
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    // Must not panic regardless of who wins the shard lock.
                    if reg.remove_validated(id, &eid).is_some() {
                        winners.fetch_add(1, Ordering::SeqCst);
                    }
                }));
            }
            for h in handles {
                h.join().expect("racer thread must not panic (issue #203)");
            }
            assert_eq!(
                winners.load(Ordering::SeqCst),
                1,
                "exactly one concurrent consumer wins the export",
            );
            assert!(!reg.validate_open(id), "export fully consumed");
        }
    }
}
