//! ADR 0009 Phase 3 integration test.
//!
//! Exercises the full reconcile pipeline at the trait level (no
//! WS round-trip, no real backend, no Postgres — those are covered
//! by the dev-vm end-to-end smoke). What this file pins down is the
//! load-bearing decision logic over multiple heartbeats:
//!
//! * Sandbox present → no flip.
//! * Sandbox missing for `< grace_ticks` heartbeats → no flip, strike
//!   accumulates.
//! * Sandbox missing for `≥ grace_ticks` heartbeats:
//!   - latest snapshot `recoverable=true` → session → `Idle`.
//!   - latest snapshot `recoverable=false` (or none) → session → `Dead`.
//!
//! Closes case **B** from the ADR 0009 failure-mode taxonomy: the
//! pattern observed today where coord restarts leave Active sessions
//! pointing at sandbox_ids that no longer exist anywhere.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use engram_coordinator::reconcile::{Reconciler, DEFAULT_GRACE_TICKS};
use engram_coordinator::state::SessionEventBus;
use engram_coordinator::HostRegistry;
use engram_core::traits::MetadataStore;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::session::SessionMode;
use engram_core::types::{
    HostRecord, HostStatus, PersistedEvent, Session, SessionSpec, SessionState, SnapshotRecord,
};
use engram_core::{HostId, MetaError, SandboxId, SessionId, SnapshotId};
use parking_lot::Mutex;

/// In-memory MetadataStore stub. Covers the methods reconcile calls
/// (`list_active_sandbox_assignments_on_host`,
/// `latest_snapshot_for_session`, `get_session`,
/// `transition_session`, `assign_session_sandbox`,
/// `append_session_event`). Every other method returns a sensible
/// empty default so the trait compiles.
#[derive(Default)]
struct ReconcileMeta {
    sessions: Mutex<HashMap<SessionId, Session>>,
    snapshots: Mutex<HashMap<SessionId, Vec<SnapshotRecord>>>,
    next_event_idx: Mutex<HashMap<SessionId, i64>>,
    /// Captured emitted events for assertions.
    emitted: Mutex<Vec<(SessionId, String, serde_json::Value)>>,
}

impl ReconcileMeta {
    fn seed_active(&self, host: HostId, sandbox: SandboxId) -> SessionId {
        let id = SessionId::new();
        let now = Utc::now();
        self.sessions.lock().insert(
            id,
            Session {
                id,
                user_id: None,
                status: SessionState::Active,
                host_id: Some(host),
                sandbox_id: Some(sandbox),
                image: "localhost:5001/demo:test".into(),
                mode: SessionMode::Agent,
                created_at: now,
                last_active_at: now,
                live_disk_manifest: None,
            },
        );
        id
    }

    fn seed_recoverable_snapshot(&self, session: SessionId, recoverable: bool) {
        let snap = SnapshotRecord {
            id: SnapshotId::new(),
            session_id: Some(session),
            host_id: None,
            image_version: "test".into(),
            size_bytes: 1024,
            created_at: Utc::now(),
            last_accessed_at: Utc::now(),
            disk_manifest: Some(ManifestRef {
                manifest_id: uuid::Uuid::new_v4(),
                version: 1,
            }),
            memory_manifest: None,
            recoverable,
        };
        self.snapshots.lock().entry(session).or_default().push(snap);
    }

    fn status(&self, id: SessionId) -> SessionState {
        self.sessions.lock().get(&id).unwrap().status
    }

    fn sandbox(&self, id: SessionId) -> Option<SandboxId> {
        self.sessions.lock().get(&id).unwrap().sandbox_id
    }

    fn emitted_events(&self) -> Vec<(SessionId, String, serde_json::Value)> {
        self.emitted.lock().clone()
    }
}

#[async_trait]
impl MetadataStore for ReconcileMeta {
    async fn ping(&self) -> Result<(), MetaError> {
        Ok(())
    }
    async fn create_session(&self, _: SessionSpec) -> Result<SessionId, MetaError> {
        unimplemented!("test seeds sessions directly")
    }
    async fn create_session_created(
        &self,
        _: SessionId,
        _: SessionSpec,
        _: engram_core::HostId,
        _: engram_core::SandboxId,
    ) -> Result<(), MetaError> {
        unimplemented!("test seeds sessions directly")
    }
    async fn get_session(&self, id: SessionId) -> Result<Session, MetaError> {
        self.sessions
            .lock()
            .get(&id)
            .cloned()
            .ok_or(MetaError::NotFound)
    }
    async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
        Ok(self
            .sessions
            .lock()
            .values()
            .filter(|s| {
                matches!(
                    s.status,
                    SessionState::Pending | SessionState::Active | SessionState::Idle
                )
            })
            .cloned()
            .collect())
    }
    async fn transition_session(
        &self,
        id: SessionId,
        target: SessionState,
    ) -> Result<SessionState, MetaError> {
        let mut g = self.sessions.lock();
        let s = g.get_mut(&id).ok_or(MetaError::NotFound)?;
        let prev = s.status;
        prev.try_transition_to(target)
            .map_err(|e| MetaError::Conflict(e.to_string()))?;
        s.status = target;
        s.last_active_at = Utc::now();
        Ok(prev)
    }
    async fn assign_session_host(
        &self,
        id: SessionId,
        host_id: Option<HostId>,
    ) -> Result<(), MetaError> {
        let mut g = self.sessions.lock();
        let s = g.get_mut(&id).ok_or(MetaError::NotFound)?;
        s.host_id = host_id;
        Ok(())
    }
    async fn assign_session_sandbox(
        &self,
        id: SessionId,
        sandbox_id: Option<SandboxId>,
    ) -> Result<(), MetaError> {
        let mut g = self.sessions.lock();
        let s = g.get_mut(&id).ok_or(MetaError::NotFound)?;
        s.sandbox_id = sandbox_id;
        Ok(())
    }
    async fn upsert_host(&self, _: HostRecord) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
        Ok(Vec::new())
    }
    async fn set_host_status(&self, _: HostId, _: HostStatus) -> Result<(), MetaError> {
        Ok(())
    }
    async fn touch_host_heartbeat(
        &self,
        _: HostId,
        _: HostStatus,
        _: engram_core::types::HostCapacity,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_stale_hosts(&self, _: u64) -> Result<Vec<HostRecord>, MetaError> {
        Ok(Vec::new())
    }
    async fn mark_host_dead_and_orphan_sessions(
        &self,
        _: HostId,
    ) -> Result<Vec<(SessionId, SessionState)>, MetaError> {
        Ok(Vec::new())
    }
    async fn record_snapshot(&self, snap: SnapshotRecord) -> Result<(), MetaError> {
        // Template snapshots (session_id=None) don't appear in the
        // per-session lookup mock; ignore them. The real PG store
        // indexes by snapshot_id so it doesn't have this issue.
        if let Some(sid) = snap.session_id {
            self.snapshots.lock().entry(sid).or_default().push(snap);
        }
        Ok(())
    }
    async fn list_snapshots_for_session(
        &self,
        sid: SessionId,
    ) -> Result<Vec<SnapshotRecord>, MetaError> {
        Ok(self.snapshots.lock().get(&sid).cloned().unwrap_or_default())
    }
    async fn latest_snapshot_for_session(
        &self,
        sid: SessionId,
    ) -> Result<Option<SnapshotRecord>, MetaError> {
        Ok(self
            .snapshots
            .lock()
            .get(&sid)
            .and_then(|v| v.last().cloned()))
    }
    async fn list_live_disk_manifest_ids(&self) -> Result<Vec<uuid::Uuid>, MetaError> {
        Ok(Vec::new())
    }
    async fn list_live_memory_manifest_ids(&self) -> Result<Vec<uuid::Uuid>, MetaError> {
        Ok(Vec::new())
    }
    async fn append_session_event(
        &self,
        session_id: SessionId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<i64, MetaError> {
        let mut counters = self.next_event_idx.lock();
        let counter = counters.entry(session_id).or_insert(0);
        let idx = *counter;
        *counter += 1;
        drop(counters);
        self.emitted
            .lock()
            .push((session_id, kind.to_string(), payload));
        Ok(idx)
    }
    async fn list_session_events_since(
        &self,
        _: SessionId,
        _: i64,
        _: i64,
    ) -> Result<Vec<PersistedEvent>, MetaError> {
        Ok(Vec::new())
    }
    async fn insert_artifact(
        &self,
        _: uuid::Uuid,
        _: SessionId,
        _: &str,
        _: &str,
        _: i64,
        _: Option<&str>,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn get_artifact(
        &self,
        _: SessionId,
        _: uuid::Uuid,
    ) -> Result<Option<engram_core::types::ArtifactRow>, MetaError> {
        Ok(None)
    }
    async fn artifact_usage(&self, _: SessionId) -> Result<(i64, i64), MetaError> {
        Ok((0, 0))
    }
    async fn upsert_registry_credential(
        &self,
        _: engram_core::types::RegistryCredential,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_registry_credentials(
        &self,
    ) -> Result<Vec<engram_core::types::RegistryCredential>, MetaError> {
        Ok(Vec::new())
    }
    async fn registry_credential_for_host(
        &self,
        _: &str,
    ) -> Result<Option<engram_core::types::RegistryCredential>, MetaError> {
        Ok(None)
    }
    async fn delete_registry_credential(&self, _: &str) -> Result<(), MetaError> {
        Ok(())
    }
    // ADR 0021 P1.5a: the four harness-pack trait methods were retired with the registry.
    async fn upsert_enabled_image(
        &self,
        _: engram_core::types::EnabledImage,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn list_enabled_images(
        &self,
    ) -> Result<Vec<engram_core::types::EnabledImage>, MetaError> {
        Ok(Vec::new())
    }
    async fn get_enabled_image(
        &self,
        _: &str,
    ) -> Result<Option<engram_core::types::EnabledImage>, MetaError> {
        Ok(None)
    }
    async fn get_enabled_image_any(
        &self,
        _: &str,
    ) -> Result<Option<engram_core::types::EnabledImage>, MetaError> {
        Ok(None)
    }
    async fn soft_delete_enabled_image(
        &self,
        _: &str,
    ) -> Result<engram_core::traits::DisableEnabledImageOutcome, MetaError> {
        Ok(engram_core::traits::DisableEnabledImageOutcome::Disabled)
    }
    async fn delete_enabled_image(&self, _: &str) -> Result<(), MetaError> {
        Ok(())
    }
    async fn upsert_session_secrets(
        &self,
        _: engram_core::types::SessionSecrets,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    async fn get_session_secrets(
        &self,
        _: SessionId,
    ) -> Result<Option<engram_core::types::SessionSecrets>, MetaError> {
        Ok(None)
    }
    async fn delete_session_secrets(&self, _: SessionId) -> Result<(), MetaError> {
        Ok(())
    }
}

/// The headline scenario from the ADR 0009 rollout doc:
/// "Start coord + 1 host, create 3 active sessions (one with a
/// `recoverable=true` snapshot taken first), drop two from the
/// backend's in-memory map (simulating crash), wait 20 s, assert:
/// the one with a snapshot flipped to Idle, the other to Dead, the
/// third stayed Active."
#[tokio::test]
async fn three_active_sessions_drop_two_one_recoverable_one_not() {
    let meta = Arc::new(ReconcileMeta::default());
    let host_registry =
        HostRegistry::new(meta.clone() as Arc<dyn engram_core::traits::MetadataStore>);
    let events = Arc::new(SessionEventBus::new(64));
    let reconciler = Reconciler::new(DEFAULT_GRACE_TICKS);
    let host = HostId::new();

    // Three sessions, each tied to a distinct sandbox_id.
    let sb_recoverable = SandboxId::new();
    let sb_dead = SandboxId::new();
    let sb_present = SandboxId::new();
    let s_recoverable = meta.seed_active(host, sb_recoverable);
    let s_dead = meta.seed_active(host, sb_dead);
    let s_present = meta.seed_active(host, sb_present);

    // Only one of the sessions has a recoverable snapshot.
    meta.seed_recoverable_snapshot(s_recoverable, true);
    // Second session: a snapshot exists but isn't recoverable
    // (mimics chunks GC'd / never replicated). Reconcile should
    // still treat this as Dead.
    meta.seed_recoverable_snapshot(s_dead, false);

    // Initial state: all three sandboxes "present" in the host's
    // heartbeat. No flips expected.
    let running = vec![sb_recoverable, sb_dead, sb_present];
    let flipped = reconciler
        .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &running)
        .await;
    assert!(flipped.is_empty(), "no missing sandboxes → no flips");

    // Now simulate the crash: two sandboxes vanish from the host's
    // backend. The third stays alive. Tick the reconcile loop the
    // grace-window number of times.
    let after_crash = vec![sb_present];
    for _ in 0..(DEFAULT_GRACE_TICKS as usize - 1) {
        let flipped = reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &after_crash)
            .await;
        assert!(
            flipped.is_empty(),
            "grace window absorbs transient missing-sandbox; no flips before strike-out"
        );
    }
    // On the N-th consecutive missing heartbeat, both crashed
    // sessions cross the strike threshold and flip.
    let mut flipped = reconciler
        .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &after_crash)
        .await;
    flipped.sort();
    let mut expected = vec![s_recoverable, s_dead];
    expected.sort();
    assert_eq!(
        flipped, expected,
        "both crashed sessions flip on the same tick; the present one is untouched"
    );

    // Verify the per-session terminal status:
    assert_eq!(
        meta.status(s_recoverable),
        SessionState::Idle,
        "session with recoverable=true → Idle (resumable via cold-tier)"
    );
    assert_eq!(
        meta.status(s_dead),
        SessionState::Dead,
        "session with recoverable=false → Dead (terminal; no resume path)"
    );
    assert_eq!(
        meta.status(s_present),
        SessionState::Active,
        "session whose sandbox is still in the heartbeat must NOT flip"
    );

    // Both flipped sessions get sandbox_id cleared so a future
    // coord restart's `repopulate_routing` doesn't try to talk to
    // the dead sandbox.
    assert_eq!(meta.sandbox(s_recoverable), None);
    assert_eq!(meta.sandbox(s_dead), None);
    // The third session keeps its sandbox_id.
    assert_eq!(meta.sandbox(s_present), Some(sb_present));

    // ADR 0015 M2: each flipped session emits TWO StatusChanged
    // events — Active -> HostLost (host went away) followed by
    // HostLost -> {Idle,Dead} (resolved per snapshot recoverability).
    // Two flipped sessions × two events = four events.
    let emitted = meta.emitted_events();
    let status_changed: Vec<_> = emitted
        .iter()
        .filter(|(_, kind, _)| kind == "status_changed")
        .collect();
    assert_eq!(
        status_changed.len(),
        4,
        "two flipped sessions × (Active->HostLost + HostLost->Idle/Dead) = 4 events"
    );
    let host_lost_events = status_changed
        .iter()
        .filter(|(_, _, p)| p["to"] == "host_lost")
        .count();
    assert_eq!(
        host_lost_events, 2,
        "one Active->HostLost event per flipped session"
    );
    let idle_events = status_changed
        .iter()
        .filter(|(_, _, p)| p["to"] == "idle")
        .count();
    let dead_events = status_changed
        .iter()
        .filter(|(_, _, p)| p["to"] == "dead")
        .count();
    assert_eq!(idle_events, 1, "the recoverable session ends in Idle");
    assert_eq!(dead_events, 1, "the non-recoverable session ends in Dead");
}

/// Defends the load-bearing anti-flap property: a sandbox that's
/// missing for `grace_ticks - 1` ticks and then re-appears must NOT
/// flip. This is the "backend.list() blipped for a few ticks then
/// recovered" scenario.
#[tokio::test]
async fn re_appearing_sandbox_within_grace_does_not_flip() {
    let meta = Arc::new(ReconcileMeta::default());
    let host_registry =
        HostRegistry::new(meta.clone() as Arc<dyn engram_core::traits::MetadataStore>);
    let events = Arc::new(SessionEventBus::new(64));
    let reconciler = Reconciler::new(DEFAULT_GRACE_TICKS);
    let host = HostId::new();
    let sb = SandboxId::new();
    let session = meta.seed_active(host, sb);

    // Two consecutive missing ticks…
    for _ in 0..(DEFAULT_GRACE_TICKS - 1) {
        let flipped = reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[])
            .await;
        assert!(flipped.is_empty());
    }
    // …then the sandbox re-appears. Strikes counter must reset to 0.
    let flipped = reconciler
        .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[sb])
        .await;
    assert!(
        flipped.is_empty(),
        "re-appearance within grace does not flip"
    );
    // Even if the sandbox subsequently vanishes again, it should
    // take a fresh `grace_ticks` consecutive misses to flip — not
    // just one (since the counter is back at 0).
    for _ in 0..(DEFAULT_GRACE_TICKS - 1) {
        let flipped = reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[])
            .await;
        assert!(flipped.is_empty(), "post-recovery, strikes started over");
    }
    let flipped = reconciler
        .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[])
        .await;
    assert_eq!(flipped, vec![session], "flip on the (n+1)th total miss");
}

/// Sessions that are already in a terminal state (Dead) must not be
/// re-flipped. The reconciler's idempotency is what lets operators
/// safely manually-kill a session without racing the strike-out.
#[tokio::test]
async fn does_not_re_flip_already_terminal_sessions() {
    let meta = Arc::new(ReconcileMeta::default());
    let host_registry =
        HostRegistry::new(meta.clone() as Arc<dyn engram_core::traits::MetadataStore>);
    let events = Arc::new(SessionEventBus::new(64));
    let reconciler = Reconciler::new(DEFAULT_GRACE_TICKS);
    let host = HostId::new();
    let sb = SandboxId::new();
    let session = meta.seed_active(host, sb);

    // First strike-out cycle → flip to Dead (no snapshot).
    for _ in 0..DEFAULT_GRACE_TICKS {
        reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[])
            .await;
    }
    assert_eq!(meta.status(session), SessionState::Dead);
    let after_first_flip = meta.emitted_events().len();

    // Even though the session row still exists, `list_active...`
    // filters status='active' so it won't be returned. Quick
    // sanity check: another N reconcile passes don't move it
    // further or emit additional events.
    for _ in 0..(DEFAULT_GRACE_TICKS * 2) {
        reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[])
            .await;
    }
    assert_eq!(meta.status(session), SessionState::Dead);
    assert_eq!(
        meta.emitted_events().len(),
        after_first_flip,
        "no further events after a session reaches a terminal state"
    );
}

/// A session on a *different* host must not be affected by a
/// reconcile pass for this host. This is the basic isolation
/// property: per-host reconciliation must not cross-contaminate.
#[tokio::test]
async fn reconcile_does_not_flip_sessions_on_other_hosts() {
    let meta = Arc::new(ReconcileMeta::default());
    let host_registry =
        HostRegistry::new(meta.clone() as Arc<dyn engram_core::traits::MetadataStore>);
    let events = Arc::new(SessionEventBus::new(64));
    let reconciler = Reconciler::new(DEFAULT_GRACE_TICKS);
    let host_a = HostId::new();
    let host_b = HostId::new();

    let sb_a = SandboxId::new();
    let sb_b = SandboxId::new();
    let session_a = meta.seed_active(host_a, sb_a);
    let session_b = meta.seed_active(host_b, sb_b);

    // Drive host_a's reconcile pass repeatedly with NO running
    // sandboxes. session_a should flip to Dead; session_b stays
    // Active because its host (B) never reported anything.
    for _ in 0..DEFAULT_GRACE_TICKS {
        reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host_a, &[])
            .await;
    }
    assert_eq!(meta.status(session_a), SessionState::Dead);
    assert_eq!(
        meta.status(session_b),
        SessionState::Active,
        "host_a's reconcile must not affect host_b's sessions"
    );
}

/// ADR 0015 M3: when the reconcile pass flips Active → HostLost,
/// the corresponding HostRegistry `sandbox_owner` entry must be
/// gone. Without this, a subsequent `exec_stream(sb)` would take
/// the cache fast path against a host whose session PG already
/// knows is HostLost, returning a tcp-connect-error or a stale
/// NotFound instead of a clean 410.
#[tokio::test]
async fn reconcile_invalidates_host_registry_cache_on_host_lost() {
    let meta = Arc::new(ReconcileMeta::default());
    let host_registry =
        HostRegistry::new(meta.clone() as Arc<dyn engram_core::traits::MetadataStore>);
    let events = Arc::new(SessionEventBus::new(64));
    let reconciler = Reconciler::new(DEFAULT_GRACE_TICKS);
    let host = HostId::new();
    let sb_lost = SandboxId::new();
    let sb_alive = SandboxId::new();
    meta.seed_active(host, sb_lost);
    meta.seed_active(host, sb_alive);

    // Mirror what create_for_session would have done at session
    // create time: seed the in-memory ownership cache.
    host_registry.record_sandbox_owner(sb_lost, host);
    host_registry.record_sandbox_owner(sb_alive, host);

    // sb_alive keeps showing up in the heartbeat; sb_lost vanishes.
    for _ in 0..DEFAULT_GRACE_TICKS {
        reconciler
            .reconcile_with_deps(meta.as_ref(), &events, &host_registry, host, &[sb_alive])
            .await;
    }

    assert_eq!(
        host_registry.host_of(sb_lost),
        None,
        "the flipped sandbox's cache row must be purged so a stale exec gets a 410, not a 404"
    );
    assert_eq!(
        host_registry.host_of(sb_alive),
        Some(host),
        "the alive sandbox's cache row must survive — selective per-sandbox invalidation"
    );
}
