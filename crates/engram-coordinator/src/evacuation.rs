//! ADR 0018 Phase A: session evacuation primitive.
//!
//! Orchestrates "move sandbox X off its current host onto
//! `target_host`": source-side snapshot → target-side restore → PG
//! rebind → routing-cache invalidation. The mechanic lives here so all
//! three callers — the admin endpoint (commit 7), the dead-host
//! second-stage trigger (commit 3), and the FC NBD-loss trigger
//! (commit 5) — share one implementation.
//!
//! The primitive leaves the session at `Created` on the new host: the
//! restored VM has snapshotted memory but a stale harness (its vsock
//! to the source's agentd died with the original sandbox). Callers
//! finish the resume-shape dance by running `start_agent` against the
//! new sandbox + transitioning to `Active`, exactly the way
//! `resume_from_fc_snapshot` in `api/snapshot.rs` finishes a resume.
//! Keeping that step outside the primitive is what lets it be unit-
//! tested against mocks without spinning up SharedState.
//!
//! Phase A scope is the *alive-source* path only: `resolve_owner` must
//! succeed against the cache/PG. The dead-source path lives in
//! `dead_host.rs` (Phase B) — it restores from a recorded snapshot
//! row instead of taking a fresh one.

use std::sync::Arc;

use engram_core::traits::MetadataStore;
use engram_core::types::evacuation::{EvacLoss, EvacReceipt};
use engram_core::types::session::SessionState;
use engram_core::{HostId, MetaError, SandboxError, SandboxId, SessionId};

use crate::host_registry::HostRegistry;

/// Errors specific to the evacuation primitive. Wraps the upstream
/// `SandboxError` / `MetaError` so callers can distinguish which step
/// failed for retry / telemetry decisions.
#[derive(Debug)]
pub enum EvacError {
    TargetIsSource { target: HostId },
    TargetNotRegistered { target: HostId },
    SourceLookup(SandboxError),
    SnapshotFailed(SandboxError),
    RestoreFailed(SandboxError),
    Rebind(MetaError),
}

impl std::fmt::Display for EvacError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TargetIsSource { target } => {
                write!(f, "target host {target} is the source host — no-op")
            }
            Self::TargetNotRegistered { target } => {
                write!(f, "target host {target} is not registered")
            }
            Self::SourceLookup(e) => write!(f, "source-side lookup failed: {e}"),
            Self::SnapshotFailed(e) => write!(f, "source-side snapshot failed: {e}"),
            Self::RestoreFailed(e) => write!(f, "target-side restore failed: {e}"),
            Self::Rebind(e) => write!(f, "PG rebind failed: {e}"),
        }
    }
}

impl std::error::Error for EvacError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TargetIsSource { .. } | Self::TargetNotRegistered { .. } => None,
            Self::SourceLookup(e) | Self::SnapshotFailed(e) | Self::RestoreFailed(e) => Some(e),
            Self::Rebind(e) => Some(e),
        }
    }
}

/// Mechanics for alive-source evacuation.
///
/// Source-side snapshot is the failure surface that has to actually
/// work for evac to succeed. If it fails (FC stuck, NBD-loss, OOM),
/// the call returns `SnapshotFailed` and nothing else changed.
///
/// Restore failures roll back the source snapshot via `abort_snapshot`
/// (best-effort) and leave the source still owning the sandbox.
///
/// PG rebind failures past the restore leave the new sandbox up on
/// the target with no PG row pointing at it — an orphan that the
/// idle evictor / host-agent restart will reap. The source still has
/// the original; the caller can re-issue evacuate after fixing the
/// PG-side problem. This window is small (three sequential PG writes)
/// and the failure is loud.
pub async fn evacuate_to(
    registry: &Arc<HostRegistry>,
    meta: &Arc<dyn MetadataStore>,
    session_id: SessionId,
    sandbox_id: SandboxId,
    target_host: HostId,
) -> Result<EvacReceipt, EvacError> {
    // 1. Source lookup. Goes through HostRegistry's cache + PG
    //    read-through. Returns HostLost-class statuses as
    //    SandboxError::HostLost — Phase A's contract is alive-source
    //    only, so callers route HostLost cases to the dead-source
    //    path (Phase B).
    let (source_host_id, source_backend) = registry
        .resolve_owner(sandbox_id)
        .await
        .map_err(EvacError::SourceLookup)?;
    if source_host_id == target_host {
        return Err(EvacError::TargetIsSource {
            target: target_host,
        });
    }
    let target_backend = registry
        .backend_of(target_host)
        .ok_or(EvacError::TargetNotRegistered {
            target: target_host,
        })?;

    // 2. Source-side: snapshot. Captures memory + disk via the
    //    existing host.snapshot() flow. ADR 0016 Phase B's continuous
    //    sync means the disk manifest the source records here is
    //    consistent with the on-VM filesystem; the freshness gain over
    //    plain restore-from-most-recent-snapshot is the memory state.
    let snapshot_metadata = source_backend
        .snapshot(sandbox_id)
        .await
        .map_err(EvacError::SnapshotFailed)?;
    // ADR 0014 issue #1/#2 commit-phase contract. Backends that need
    // it (FC) reify the manifest into BlobStorage here; default Ok
    // for others (Process, VZ-dev). Failure is non-fatal — the
    // chunk-GC 24h grace window keeps the chunks alive long enough
    // for restore to rehydrate even if commit_snapshot didn't finish.
    if let Err(e) = source_backend.commit_snapshot(sandbox_id).await {
        tracing::warn!(
            %session_id,
            %sandbox_id,
            error = %e,
            "evac: commit_snapshot on source failed; continuing — \
             chunk-GC grace covers the restore window",
        );
    }

    // 3. Target-side: restore. Brings up the snapshotted VM on the
    //    target host. Returns the new SandboxId — different from the
    //    source's per ADR 0015 §M4 ("the sandbox_id token can be
    //    substituted").
    let new_sandbox_id = match target_backend.restore(snapshot_metadata).await {
        Ok(id) => id,
        Err(e) => {
            // Rollback: abort the source-side snapshot so its staging
            // dir is cleaned up. Best-effort; the host-agent restart
            // sweep will reap whatever this misses.
            let _ = source_backend.abort_snapshot(sandbox_id).await;
            return Err(EvacError::RestoreFailed(e));
        }
    };

    // 4. Routing cache: invalidate the old binding before any reader
    //    races us on the PG row. Insert the new binding so subsequent
    //    RPCs against new_sandbox_id route to target.
    registry.invalidate_sandbox(sandbox_id);
    registry.record_sandbox_owner(new_sandbox_id, target_host);

    // 5. PG rebind. State-machine drive: Active → HostLost → Created.
    //    The caller (admin endpoint or auto-trigger) finishes the
    //    transition to Active after running start_agent.
    //
    //    Auto-trigger paths (dead_host.rs second-stage) may have
    //    already flipped the session to HostLost via the bulk
    //    `mark_host_dead_and_orphan_sessions` query — skip the first
    //    step in that case.
    let session = meta
        .get_session(session_id)
        .await
        .map_err(EvacError::Rebind)?;
    if matches!(session.status, SessionState::Active) {
        meta.transition_session(session_id, SessionState::HostLost)
            .await
            .map_err(EvacError::Rebind)?;
    }
    meta.assign_session_host(session_id, Some(target_host))
        .await
        .map_err(EvacError::Rebind)?;
    meta.assign_session_sandbox(session_id, Some(new_sandbox_id))
        .await
        .map_err(EvacError::Rebind)?;
    meta.transition_session(session_id, SessionState::Created)
        .await
        .map_err(EvacError::Rebind)?;

    // 6. Source-side cleanup: best-effort destroy of the orphaned
    //    sandbox. Failure logs at warn — the source host-agent's
    //    startup sweep (ADR 0017) reaps any leftover state.
    if let Err(e) = source_backend.destroy(sandbox_id).await {
        tracing::warn!(
            %session_id,
            %sandbox_id,
            %source_host_id,
            error = %e,
            "evac: source-side destroy failed; orphan will be reaped on next host-agent restart",
        );
    }

    Ok(EvacReceipt {
        new_host_id: target_host,
        new_sandbox_id,
        loss: EvacLoss::None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use engram_core::traits::{HarnessDial, HostClient};
    use engram_core::types::sandbox::{ExecRequest, ExecStream, SandboxSpec};
    use engram_core::types::session::{HarnessSpec, Session, SessionState};
    use engram_core::types::snapshot::SnapshotMetadata;
    use parking_lot::Mutex as PlMutex;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records every call made against it; deterministic returns so
    /// the orchestration's call ordering is observable in tests.
    /// Snapshot returns a SnapshotMetadata with deterministic UUID;
    /// restore returns the pre-staged `next_restore_id`.
    #[derive(Default)]
    struct FakeBackend {
        calls: PlMutex<Vec<String>>,
        next_restore_id: PlMutex<Option<SandboxId>>,
        fail_snapshot: AtomicUsize, // 0=ok, 1=fail
        fail_restore: AtomicUsize,
    }

    impl FakeBackend {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().clone()
        }
        fn set_restore_id(&self, id: SandboxId) {
            *self.next_restore_id.lock() = Some(id);
        }
    }

    #[async_trait]
    impl HostClient for FakeBackend {
        async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
            self.calls.lock().push("create".into());
            Ok(SandboxId::new())
        }
        async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.calls.lock().push(format!("destroy:{id}"));
            Ok(())
        }
        async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
            Ok(vec![])
        }
        async fn exec_stream(
            &self,
            _id: SandboxId,
            _cmd: ExecRequest,
        ) -> Result<ExecStream, SandboxError> {
            unreachable!()
        }
        async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            self.calls.lock().push(format!("snapshot:{id}"));
            if self.fail_snapshot.load(Ordering::SeqCst) > 0 {
                return Err(SandboxError::Vm(Box::new(SimpleErr("snapshot failed".into()))));
            }
            Ok(SnapshotMetadata {
                id: engram_core::SnapshotId::new(),
                size_bytes: 1024,
                created_at: chrono::Utc::now(),
                image_version: "test".into(),
                disk_manifest: None,
                memory_manifest: None,
                source_sandbox_id: None,
                state_blob_key: None,
                sidecar_blob_key: None,
                rootfs_blob_key: None,
                working_set_blob_key: None,
            })
        }
        async fn commit_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.calls.lock().push(format!("commit_snapshot:{id}"));
            Ok(())
        }
        async fn abort_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
            self.calls.lock().push(format!("abort_snapshot:{id}"));
            Ok(())
        }
        async fn restore(&self, _md: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
            self.calls.lock().push("restore".into());
            if self.fail_restore.load(Ordering::SeqCst) > 0 {
                return Err(SandboxError::Vm(Box::new(SimpleErr("restore failed".into()))));
            }
            Ok(self
                .next_restore_id
                .lock()
                .unwrap_or_else(|| SandboxId::new()))
        }
        async fn start_agent(
            &self,
            _id: SandboxId,
            _agent: engram_core::types::sandbox::AgentSpec,
            _policy: engram_core::types::egress::SessionEgressPolicy,
        ) -> Result<(), SandboxError> {
            unreachable!("evac primitive does not call start_agent")
        }
        async fn apply_egress_policy(
            &self,
            _policy: engram_core::types::egress::SessionEgressPolicy,
        ) -> Result<(), SandboxError> {
            unreachable!()
        }
        async fn guest_ip(&self, _id: SandboxId) -> Option<String> {
            None
        }
        async fn bind_session(&self, _session_id: SessionId, _sandbox_id: SandboxId) {}
        async fn unbind_session(&self, _session_id: SessionId) {}
        async fn send_prompt(
            &self,
            _sandbox_id: SandboxId,
            _text: String,
        ) -> Result<(), SandboxError> {
            unreachable!()
        }
        async fn acquire_shell(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
            unreachable!()
        }
        async fn release_shell(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
            unreachable!()
        }
        fn harness_dial(&self) -> HarnessDial {
            HarnessDial::Vsock
        }
    }

    #[derive(Debug)]
    struct SimpleErr(String);
    impl std::fmt::Display for SimpleErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for SimpleErr {}

    /// Minimal MetadataStore that records every state-affecting call.
    /// Stores one session row keyed by SessionId; the evac orchestration
    /// reads it via `get_session` and updates via the assign_* /
    /// transition_session calls.
    #[derive(Default)]
    struct FakeMeta {
        sessions: PlMutex<HashMap<SessionId, Session>>,
        sandbox_to_session: PlMutex<HashMap<SandboxId, (HostId, SessionState)>>,
        calls: PlMutex<Vec<String>>,
    }

    impl FakeMeta {
        fn install_session(&self, sess: Session) {
            if let (Some(_), Some(sb)) = (sess.host_id, sess.sandbox_id) {
                self.sandbox_to_session
                    .lock()
                    .insert(sb, (sess.host_id.unwrap(), sess.status));
            }
            self.sessions.lock().insert(sess.id, sess);
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().clone()
        }
        fn session(&self, id: SessionId) -> Session {
            self.sessions.lock().get(&id).cloned().unwrap()
        }
    }

    #[async_trait]
    impl MetadataStore for FakeMeta {
        async fn create_session(
            &self,
            _: engram_core::types::session::SessionSpec,
        ) -> Result<SessionId, MetaError> {
            unreachable!()
        }
        async fn create_session_created(
            &self,
            _: SessionId,
            _: engram_core::types::session::SessionSpec,
            _: HostId,
            _: SandboxId,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn get_session(&self, id: SessionId) -> Result<Session, MetaError> {
            self.calls.lock().push("get_session".into());
            self.sessions
                .lock()
                .get(&id)
                .cloned()
                .ok_or(MetaError::NotFound)
        }
        async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
            Ok(self.sessions.lock().values().cloned().collect())
        }
        async fn transition_session(
            &self,
            id: SessionId,
            target: SessionState,
        ) -> Result<SessionState, MetaError> {
            self.calls
                .lock()
                .push(format!("transition_session:{target:?}"));
            let mut guard = self.sessions.lock();
            let s = guard.get_mut(&id).ok_or(MetaError::NotFound)?;
            let prev = s.status;
            s.status
                .try_transition_to(target)
                .map_err(|e| MetaError::Conflict(e.to_string()))?;
            s.status = target;
            Ok(prev)
        }
        async fn assign_session_host(
            &self,
            id: SessionId,
            host_id: Option<HostId>,
        ) -> Result<(), MetaError> {
            self.calls
                .lock()
                .push(format!("assign_session_host:{host_id:?}"));
            self.sessions
                .lock()
                .get_mut(&id)
                .ok_or(MetaError::NotFound)?
                .host_id = host_id;
            Ok(())
        }
        async fn assign_session_sandbox(
            &self,
            id: SessionId,
            sandbox_id: Option<SandboxId>,
        ) -> Result<(), MetaError> {
            self.calls
                .lock()
                .push(format!("assign_session_sandbox:{sandbox_id:?}"));
            self.sessions
                .lock()
                .get_mut(&id)
                .ok_or(MetaError::NotFound)?
                .sandbox_id = sandbox_id;
            Ok(())
        }
        async fn host_for_sandbox(
            &self,
            sandbox_id: SandboxId,
        ) -> Result<Option<(HostId, SessionState)>, MetaError> {
            Ok(self.sandbox_to_session.lock().get(&sandbox_id).copied())
        }
        async fn upsert_host(
            &self,
            _: engram_core::types::host::HostRecord,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn list_active_hosts(
            &self,
        ) -> Result<Vec<engram_core::types::host::HostRecord>, MetaError> {
            Ok(Vec::new())
        }
        async fn set_host_status(
            &self,
            _: HostId,
            _: engram_core::types::host::HostStatus,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn touch_host_heartbeat(
            &self,
            _: HostId,
            _: engram_core::types::host::HostStatus,
            _: engram_core::types::host::HostCapacity,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn list_stale_hosts(
            &self,
            _: u64,
        ) -> Result<Vec<engram_core::types::host::HostRecord>, MetaError> {
            Ok(Vec::new())
        }
        async fn mark_host_dead_and_orphan_sessions(
            &self,
            _: HostId,
        ) -> Result<Vec<(SessionId, SessionState)>, MetaError> {
            Ok(Vec::new())
        }
        async fn record_snapshot(
            &self,
            _: engram_core::types::snapshot::SnapshotRecord,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn list_snapshots_for_session(
            &self,
            _: SessionId,
        ) -> Result<Vec<engram_core::types::snapshot::SnapshotRecord>, MetaError> {
            Ok(Vec::new())
        }
        async fn latest_snapshot_for_session(
            &self,
            _: SessionId,
        ) -> Result<Option<engram_core::types::snapshot::SnapshotRecord>, MetaError> {
            Ok(None)
        }
        async fn append_session_event(
            &self,
            _: SessionId,
            _: &str,
            _: serde_json::Value,
        ) -> Result<i64, MetaError> {
            Ok(0)
        }
        async fn list_session_events_since(
            &self,
            _: SessionId,
            _: i64,
            _: i64,
        ) -> Result<Vec<engram_core::types::event::PersistedEvent>, MetaError> {
            Ok(Vec::new())
        }
        async fn upsert_registry_credential(
            &self,
            _: engram_core::types::registry::RegistryCredential,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn list_registry_credentials(
            &self,
        ) -> Result<Vec<engram_core::types::registry::RegistryCredential>, MetaError> {
            Ok(Vec::new())
        }
        async fn registry_credential_for_host(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::registry::RegistryCredential>, MetaError> {
            Ok(None)
        }
        async fn delete_registry_credential(&self, _: &str) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn upsert_harness_pack(
            &self,
            _: engram_core::types::registry::HarnessPack,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn list_harness_packs(
            &self,
        ) -> Result<Vec<engram_core::types::registry::HarnessPack>, MetaError> {
            Ok(Vec::new())
        }
        async fn get_harness_pack(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::registry::HarnessPack>, MetaError> {
            Ok(None)
        }
        async fn delete_harness_pack(&self, _: &str) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn upsert_enabled_image(
            &self,
            _: engram_core::types::registry::EnabledImage,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn list_enabled_images(
            &self,
        ) -> Result<Vec<engram_core::types::registry::EnabledImage>, MetaError> {
            Ok(Vec::new())
        }
        async fn get_enabled_image(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::registry::EnabledImage>, MetaError> {
            Ok(None)
        }
        async fn delete_enabled_image(&self, _: &str) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn upsert_session_secrets(
            &self,
            _: engram_core::types::registry::SessionSecrets,
        ) -> Result<(), MetaError> {
            unreachable!()
        }
        async fn get_session_secrets(
            &self,
            _: SessionId,
        ) -> Result<Option<engram_core::types::registry::SessionSecrets>, MetaError> {
            Ok(None)
        }
        async fn delete_session_secrets(&self, _: SessionId) -> Result<(), MetaError> {
            unreachable!()
        }
    }

    fn make_session(host: HostId, sandbox: SandboxId, status: SessionState) -> Session {
        Session {
            id: SessionId::new(),
            user_id: None,
            status,
            host_id: Some(host),
            sandbox_id: Some(sandbox),
            image: "ghcr.io/test/img:t".into(),
            harness: HarnessSpec::None,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        }
    }

    /// Happy path: alive Active source, snapshot succeeds, restore
    /// succeeds, rebind drives through Active → HostLost → Created.
    /// The call ordering is the load-bearing contract — read the
    /// `calls` vec to verify the orchestration ran the steps in the
    /// right order.
    #[tokio::test]
    async fn evacuate_to_happy_path() {
        let source_host = HostId::new();
        let target_host = HostId::new();
        let old_sandbox = SandboxId::new();
        let new_sandbox = SandboxId::new();

        let meta = Arc::new(FakeMeta::default());
        let session = make_session(source_host, old_sandbox, SessionState::Active);
        let session_id = session.id;
        meta.install_session(session);

        let registry = Arc::new(HostRegistry::new(meta.clone()));
        let source_be = Arc::new(FakeBackend::default());
        let target_be = Arc::new(FakeBackend::default());
        target_be.set_restore_id(new_sandbox);
        registry.register(source_host, source_be.clone());
        registry.register(target_host, target_be.clone());
        registry.record_sandbox_owner(old_sandbox, source_host);

        let receipt = evacuate_to(&registry, &(meta.clone() as Arc<dyn MetadataStore>), session_id, old_sandbox, target_host)
            .await
            .expect("happy path should succeed");

        assert_eq!(receipt.new_host_id, target_host);
        assert_eq!(receipt.new_sandbox_id, new_sandbox);
        assert_eq!(receipt.loss, EvacLoss::None);

        // Source-side calls: snapshot, commit_snapshot, destroy.
        let src_calls = source_be.calls();
        assert!(
            src_calls.iter().any(|c| c.starts_with("snapshot:")),
            "source should snapshot: {src_calls:?}",
        );
        assert!(
            src_calls
                .iter()
                .any(|c| c.starts_with("commit_snapshot:")),
            "source should commit_snapshot: {src_calls:?}",
        );
        assert!(
            src_calls.iter().any(|c| c.starts_with("destroy:")),
            "source should destroy after rebind: {src_calls:?}",
        );

        // Target-side: just restore.
        assert_eq!(target_be.calls(), vec!["restore".to_string()]);

        // PG mechanics: state moved Active → HostLost → Created, host
        // and sandbox reassigned.
        let updated = meta.session(session_id);
        assert_eq!(updated.status, SessionState::Created);
        assert_eq!(updated.host_id, Some(target_host));
        assert_eq!(updated.sandbox_id, Some(new_sandbox));

        // Routing cache: old gone, new pointed at target.
        assert_eq!(registry.host_of(new_sandbox), Some(target_host));
        assert_eq!(registry.host_of(old_sandbox), None);
    }

    /// Source-side snapshot fails: caller gets SnapshotFailed and PG
    /// is untouched.
    #[tokio::test]
    async fn evacuate_to_snapshot_failure_leaves_pg_unchanged() {
        let source_host = HostId::new();
        let target_host = HostId::new();
        let old_sandbox = SandboxId::new();

        let meta = Arc::new(FakeMeta::default());
        let session = make_session(source_host, old_sandbox, SessionState::Active);
        let session_id = session.id;
        meta.install_session(session);

        let registry = Arc::new(HostRegistry::new(meta.clone()));
        let source_be = Arc::new(FakeBackend::default());
        source_be.fail_snapshot.store(1, Ordering::SeqCst);
        let target_be = Arc::new(FakeBackend::default());
        registry.register(source_host, source_be.clone());
        registry.register(target_host, target_be.clone());
        registry.record_sandbox_owner(old_sandbox, source_host);

        let result =
            evacuate_to(&registry, &(meta.clone() as Arc<dyn MetadataStore>), session_id, old_sandbox, target_host).await;
        assert!(matches!(result, Err(EvacError::SnapshotFailed(_))));

        // No restore on target.
        assert!(target_be.calls().is_empty());
        // PG untouched: still Active, original host/sandbox.
        let updated = meta.session(session_id);
        assert_eq!(updated.status, SessionState::Active);
        assert_eq!(updated.host_id, Some(source_host));
        assert_eq!(updated.sandbox_id, Some(old_sandbox));
        // Routing cache: old binding still present.
        assert_eq!(registry.host_of(old_sandbox), Some(source_host));
    }

    /// Target-side restore fails: abort_snapshot fires on source, PG
    /// stays untouched, and routing cache still points at source.
    #[tokio::test]
    async fn evacuate_to_restore_failure_aborts_source_snapshot() {
        let source_host = HostId::new();
        let target_host = HostId::new();
        let old_sandbox = SandboxId::new();

        let meta = Arc::new(FakeMeta::default());
        let session = make_session(source_host, old_sandbox, SessionState::Active);
        let session_id = session.id;
        meta.install_session(session);

        let registry = Arc::new(HostRegistry::new(meta.clone()));
        let source_be = Arc::new(FakeBackend::default());
        let target_be = Arc::new(FakeBackend::default());
        target_be.fail_restore.store(1, Ordering::SeqCst);
        registry.register(source_host, source_be.clone());
        registry.register(target_host, target_be.clone());
        registry.record_sandbox_owner(old_sandbox, source_host);

        let result =
            evacuate_to(&registry, &(meta.clone() as Arc<dyn MetadataStore>), session_id, old_sandbox, target_host).await;
        assert!(matches!(result, Err(EvacError::RestoreFailed(_))));

        // Source-side: snapshot ran, then abort_snapshot ran.
        let src = source_be.calls();
        assert!(src.iter().any(|c| c.starts_with("abort_snapshot:")), "{src:?}");
        // No destroy — the source still owns the sandbox.
        assert!(src.iter().all(|c| !c.starts_with("destroy:")), "{src:?}");

        // PG untouched.
        let updated = meta.session(session_id);
        assert_eq!(updated.status, SessionState::Active);
        assert_eq!(updated.host_id, Some(source_host));
        assert_eq!(updated.sandbox_id, Some(old_sandbox));
    }

    /// Target == source: TargetIsSource returned immediately. No
    /// side effects.
    #[tokio::test]
    async fn evacuate_to_rejects_same_host_target() {
        let source_host = HostId::new();
        let old_sandbox = SandboxId::new();

        let meta = Arc::new(FakeMeta::default());
        let session = make_session(source_host, old_sandbox, SessionState::Active);
        let session_id = session.id;
        meta.install_session(session);

        let registry = Arc::new(HostRegistry::new(meta.clone()));
        let source_be = Arc::new(FakeBackend::default());
        registry.register(source_host, source_be.clone());
        registry.record_sandbox_owner(old_sandbox, source_host);

        let result =
            evacuate_to(&registry, &(meta.clone() as Arc<dyn MetadataStore>), session_id, old_sandbox, source_host).await;
        assert!(matches!(result, Err(EvacError::TargetIsSource { .. })));
        assert!(source_be.calls().is_empty(), "no calls on source");
    }

    /// Target host not registered: TargetNotRegistered returned
    /// before any RPC fires.
    #[tokio::test]
    async fn evacuate_to_rejects_unknown_target() {
        let source_host = HostId::new();
        let bogus_target = HostId::new();
        let old_sandbox = SandboxId::new();

        let meta = Arc::new(FakeMeta::default());
        let session = make_session(source_host, old_sandbox, SessionState::Active);
        let session_id = session.id;
        meta.install_session(session);

        let registry = Arc::new(HostRegistry::new(meta.clone()));
        let source_be = Arc::new(FakeBackend::default());
        registry.register(source_host, source_be.clone());
        registry.record_sandbox_owner(old_sandbox, source_host);

        let result =
            evacuate_to(&registry, &(meta.clone() as Arc<dyn MetadataStore>), session_id, old_sandbox, bogus_target).await;
        assert!(matches!(result, Err(EvacError::TargetNotRegistered { .. })));
        assert!(source_be.calls().is_empty(), "no calls on source");
    }

    /// Session already in HostLost (e.g. dead_host.rs flipped it
    /// before calling here): orchestration skips the Active → HostLost
    /// transition and goes straight to assign_* / Created.
    #[tokio::test]
    async fn evacuate_to_skips_host_lost_step_when_already_host_lost() {
        let source_host = HostId::new();
        let target_host = HostId::new();
        let old_sandbox = SandboxId::new();
        let new_sandbox = SandboxId::new();

        let meta = Arc::new(FakeMeta::default());
        // Session in HostLost — like dead_host.rs already flipped it.
        // host_id / sandbox_id stay populated for resolve_owner to work
        // on the routing cache side; resolve_owner consults PG too but
        // our FakeMeta returns HostLost which triggers HostLost in the
        // resolver. So bias this test by populating routing cache and
        // setting status before calling.
        let session = make_session(source_host, old_sandbox, SessionState::HostLost);
        let session_id = session.id;
        meta.install_session(session);

        let registry = Arc::new(HostRegistry::new(meta.clone()));
        let source_be = Arc::new(FakeBackend::default());
        let target_be = Arc::new(FakeBackend::default());
        target_be.set_restore_id(new_sandbox);
        registry.register(source_host, source_be.clone());
        registry.register(target_host, target_be.clone());
        // Cache hit path: resolve_owner short-circuits PG when the
        // cache row exists and the host entry is fresh. Otherwise PG
        // would return HostLost status and the resolver would error.
        registry.record_sandbox_owner(old_sandbox, source_host);

        let receipt =
            evacuate_to(&registry, &(meta.clone() as Arc<dyn MetadataStore>), session_id, old_sandbox, target_host)
                .await
                .expect("HostLost path should succeed via cache hit");

        assert_eq!(receipt.loss, EvacLoss::None);

        // Crucial assertion: no Active → HostLost transition recorded.
        // Only HostLost → Created.
        let transition_calls: Vec<_> = meta
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("transition_session:"))
            .collect();
        assert_eq!(transition_calls.len(), 1, "{transition_calls:?}");
        assert!(transition_calls[0].contains("Created"));
    }
}
