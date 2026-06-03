//! ADR 0018: dead-source session evacuation primitive.
//!
//! `evacuate_dead_source` restores a session onto a peer host from
//! already-recorded artifacts (a snapshot row and/or the live disk
//! manifest) when the source host is gone. The two callers — the
//! `evac_resumer` background scanner (driving `Evacuating → Created`)
//! and the FC NBD-loss trigger — share this one implementation.
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
//! ADR 0018 commit 12h retired the synchronous *alive-source*
//! `evacuate_to` primitive: the async rework (Evacuating state +
//! scanner) made it dead code. All evac now flows through the
//! state-machine + `evacuate_dead_source` resume path.

use std::sync::Arc;

use engram_core::traits::MetadataStore;
use engram_core::types::evacuation::{EvacLoss, EvacReceipt};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::session::{Session, SessionState};
use engram_core::types::snapshot::{SnapshotMetadata, SnapshotRecord};
use engram_core::{MetaError, SandboxError};

use crate::host_registry::{HostRegistry, PickError, ScheduleContext};

/// Errors specific to the evacuation primitive. Wraps the upstream
/// `SandboxError` / `MetaError` so callers can distinguish which step
/// failed for retry / telemetry decisions.
#[derive(Debug)]
pub enum EvacError {
    RestoreFailed(SandboxError),
    Rebind(MetaError),
    /// No recoverable state to restore from. Dead-source path returns
    /// this when both `snapshot` and `session.live_disk_manifest` are
    /// `None`. Caller routes to `HostLost → Dead`.
    NoRecoverableState,
    /// No host could accept the relocate (no capacity, or no host
    /// with the image prefetched). Caller logs + retries later or
    /// routes to `HostLost → Dead`.
    NoTargetAvailable(PickError),
}

impl std::fmt::Display for EvacError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RestoreFailed(e) => write!(f, "target-side restore failed: {e}"),
            Self::Rebind(e) => write!(f, "PG rebind failed: {e}"),
            Self::NoRecoverableState => {
                write!(
                    f,
                    "no snapshot or live disk manifest — session cannot be evacuated"
                )
            }
            Self::NoTargetAvailable(e) => write!(f, "no host could accept the relocate: {e:?}"),
        }
    }
}

impl std::error::Error for EvacError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoRecoverableState | Self::NoTargetAvailable(_) => None,
            Self::RestoreFailed(e) => Some(e),
            Self::Rebind(e) => Some(e),
        }
    }
}

/// Pick the disk manifest the target should restore from. Mirrors the
/// `effective_resume_disk_manifest` semantics in `api/snapshot.rs`:
/// when both live and snapshot manifests exist, prefer the live one
/// only if it's a strictly newer version of the same manifest_id
/// lineage; otherwise the snapshot wins (different lineage means we
/// trust the (memory, disk) pair the snapshot captured together).
fn pick_evac_disk_manifest(
    live: Option<ManifestRef>,
    snapshot: Option<ManifestRef>,
) -> Option<ManifestRef> {
    match (live, snapshot) {
        (None, snap) => snap,
        (Some(l), None) => Some(l),
        (Some(l), Some(s)) => {
            if l.manifest_id == s.manifest_id && l.version > s.version {
                Some(l)
            } else {
                Some(s)
            }
        }
    }
}

/// Mechanics for **dead-source** evacuation. Used by `dead_host.rs`'s
/// second-stage transition (Phase B) when a host is already marked
/// Dead and the session has been flipped to `HostLost` — the source
/// backend is unreachable, so a fresh source-side snapshot isn't
/// possible.
///
/// Restores from existing artifacts:
///
/// - **Disk**: `pick_evac_disk_manifest(session.live_disk_manifest,
///   snapshot.disk_manifest)`. Live wins when newer (continuous-sync
///   captured a flush after the snapshot). Snapshot wins on different
///   lineage or older live.
/// - **Memory**: from `snapshot.memory_manifest` when a snapshot
///   exists. Memory loss is accepted when only the live disk manifest
///   is available — `EvacLoss::Memory { reason: "source-dead-no-snapshot" }`.
///
/// Failure modes:
///
/// - `snapshot.is_none() && session.live_disk_manifest.is_none()` →
///   `NoRecoverableState`. Caller drives `HostLost → Dead`.
/// - Target pick fails (no capacity, image not ready) →
///   `NoTargetAvailable`. Caller logs + may retry; nothing changed.
/// - Target-side restore fails → `RestoreFailed`. Caller may retry
///   against a different host; nothing changed.
/// - PG rebind fails after restore → new sandbox is up on target
///   without a PG row pointing at it (a small orphan window). Idle
///   evictor / host-agent restart sweep reaps.
///
/// Leaves the session at `Created` on the new host. Caller is
/// responsible for the start_agent + Active transition.
pub async fn evacuate_dead_source(
    registry: &Arc<HostRegistry>,
    meta: &Arc<dyn MetadataStore>,
    session: Session,
    snapshot: Option<SnapshotRecord>,
) -> Result<EvacReceipt, EvacError> {
    let session_id = session.id;
    let old_sandbox_id = session.sandbox_id;

    let disk_manifest = pick_evac_disk_manifest(
        session.live_disk_manifest,
        snapshot.as_ref().and_then(|s| s.disk_manifest),
    );
    let memory_manifest = snapshot.as_ref().and_then(|s| s.memory_manifest);

    if disk_manifest.is_none() && memory_manifest.is_none() {
        return Err(EvacError::NoRecoverableState);
    }

    let (loss, reason) = if memory_manifest.is_some() {
        (EvacLoss::None, "")
    } else {
        (
            EvacLoss::Memory {
                reason: "source-dead-no-snapshot".into(),
            },
            "source-dead-no-snapshot",
        )
    };
    let _ = reason; // structured-log placeholder; metric label lives on `loss.as_str()`.

    // Build the SnapshotMetadata. When we have a snapshot row, lift
    // its fields verbatim (id, size, image_version, source_sandbox_id)
    // and DERIVE the portable blob keys from the snapshot_id. The keys
    // are deterministic functions of the id
    // (`snapshots/<id>/{state.bin,sidecar.json}`), so we can rebuild
    // them at restore time without needing dedicated columns on the
    // snapshot row.
    //
    // ADR 0018 commit 12 (async evac): the source records the snapshot
    // in PG before the scanner picks the session up, which loses the
    // `state_blob_key` / `sidecar_blob_key` that
    // `PooledBackend::snapshot` populated on the in-memory metadata.
    // Deriving them here is the canonical fix — same shape as the
    // matching `materialize_state_if_missing` helper that consumes
    // them on the target. Without these, the target host's FC
    // `restore()` errors with "manifest.json: No such file or
    // directory" because nothing materialised the sidecar.
    let metadata = match snapshot.as_ref() {
        Some(s) => SnapshotMetadata {
            id: s.id,
            size_bytes: s.size_bytes,
            created_at: s.created_at,
            image_version: s.image_version.clone(),
            disk_manifest,
            memory_manifest,
            source_sandbox_id: None,
            state_blob_key: Some(engram_chunk_store::snapshot_blob::state_blob_key(s.id)),
            sidecar_blob_key: Some(engram_chunk_store::snapshot_blob::sidecar_blob_key(s.id)),
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: vec![],
        },
        None => SnapshotMetadata {
            id: engram_core::SnapshotId::new(),
            size_bytes: 0,
            created_at: chrono::Utc::now(),
            image_version: String::new(),
            disk_manifest,
            memory_manifest: None,
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: vec![],
        },
    };

    let (image_repo, image_tag) = engram_core::types::session::split_image_ref(&session.image);
    let ctx = ScheduleContext {
        repo: image_repo,
        image_version: image_tag,
        prefer_snapshot_id: snapshot.as_ref().map(|s| s.id),
        memory_mib: None,
        // Phase C target-selection: image-cache-warm preference is a
        // future refinement (defer when we add zone tagging to
        // HostState). Today we accept any host that can take the
        // work, but never the source host (exclude_host) — set to
        // the prior owner via `session.host_id` so the NBD-loss
        // trigger doesn't relocate back onto the degraded host. For
        // the dead-source path the source is already unregistered;
        // exclude_host is defensive.
        required_image_digest: None,
        exclude_host: session.host_id,
    };

    // Split pick + restore so picker errors and backend errors keep
    // distinct typing — picker failures are `NoTargetAvailable`
    // (operator action: free capacity or wait for image prefetch);
    // backend failures are `RestoreFailed` (retry against another
    // host or surface to user).
    let (target_host, target_backend) = registry
        .pick_for_session(&ctx)
        .map_err(EvacError::NoTargetAvailable)?;
    let new_sandbox_id = target_backend
        .restore(metadata)
        .await
        .map_err(EvacError::RestoreFailed)?;

    // Routing cache: invalidate the stale source binding (the source
    // host is dead, so this is usually already gone from
    // `host_registry.unregister`, but defensive). Insert the new.
    if let Some(old) = old_sandbox_id {
        registry.invalidate_sandbox(old);
    }
    registry.record_sandbox_owner(new_sandbox_id, target_host);

    // PG rebind. dead_host.rs has already flipped Active → HostLost
    // (via `mark_host_dead_and_orphan_sessions`), so we drive
    // HostLost → Created here.
    meta.assign_session_host(session_id, Some(target_host))
        .await
        .map_err(EvacError::Rebind)?;
    meta.assign_session_sandbox(session_id, Some(new_sandbox_id))
        .await
        .map_err(EvacError::Rebind)?;
    meta.transition_session(session_id, SessionState::Created)
        .await
        .map_err(EvacError::Rebind)?;

    Ok(EvacReceipt {
        new_host_id: target_host,
        new_sandbox_id,
        loss,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use engram_core::traits::{HarnessDial, HostClient};
    use engram_core::types::sandbox::{ExecRequest, ExecStream, SandboxSpec};
    use engram_core::types::session::{Session, SessionMode, SessionState};
    use engram_core::types::snapshot::SnapshotMetadata;
    use engram_core::{HostId, SandboxId, SessionId};
    use parking_lot::Mutex as PlMutex;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Records every call made against it; deterministic returns so
    /// the orchestration's call ordering is observable in tests.
    /// Snapshot returns a SnapshotMetadata with deterministic UUID;
    /// restore returns the pre-staged `next_restore_id`.
    #[derive(Default)]
    struct FakeBackend {
        next_restore_id: PlMutex<Option<SandboxId>>,
        fail_snapshot: AtomicUsize, // 0=ok, 1=fail
        fail_restore: AtomicUsize,
    }

    impl FakeBackend {
        fn set_restore_id(&self, id: SandboxId) {
            *self.next_restore_id.lock() = Some(id);
        }
    }

    #[async_trait]
    impl HostClient for FakeBackend {
        async fn create(&self, _spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
            Ok(SandboxId::new())
        }
        async fn destroy(&self, _id: SandboxId) -> Result<(), SandboxError> {
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
        async fn snapshot(&self, _id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
            if self.fail_snapshot.load(Ordering::SeqCst) > 0 {
                return Err(SandboxError::Vm(Box::new(SimpleErr(
                    "snapshot failed".into(),
                ))));
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
                aux_bundles: vec![],
            })
        }
        async fn commit_snapshot(&self, _id: SandboxId) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn abort_snapshot(&self, _id: SandboxId) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn restore(&self, _md: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
            if self.fail_restore.load(Ordering::SeqCst) > 0 {
                return Err(SandboxError::Vm(Box::new(SimpleErr(
                    "restore failed".into(),
                ))));
            }
            // FakeBackend tests always preload a restore id; the
            // fallback is just defensive against a misconfigured test.
            // Lifted out of unwrap_or_else / unwrap_or to dodge clippy's
            // unwrap_or_default lint (Default would mint a nil UUID,
            // which would mask test bugs vs. a fresh id flagging them).
            Ok(match *self.next_restore_id.lock() {
                Some(id) => id,
                None => SandboxId::new(),
            })
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
        // ADR 0021 P1.5a: the four harness-pack trait methods were retired with the registry.
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
        async fn get_enabled_image_any(
            &self,
            _: &str,
        ) -> Result<Option<engram_core::types::registry::EnabledImage>, MetaError> {
            Ok(None)
        }
        async fn soft_delete_enabled_image(
            &self,
            _: &str,
        ) -> Result<engram_core::traits::DisableEnabledImageOutcome, MetaError> {
            unreachable!()
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
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        }
    }

    fn make_snapshot_for(
        session_id: SessionId,
        disk: Option<ManifestRef>,
        memory: Option<ManifestRef>,
    ) -> SnapshotRecord {
        SnapshotRecord {
            id: engram_core::SnapshotId::new(),
            session_id: Some(session_id),
            host_id: None,
            image_version: "test".into(),
            size_bytes: 1024,
            created_at: chrono::Utc::now(),
            last_accessed_at: chrono::Utc::now(),
            disk_manifest: disk,
            memory_manifest: memory,
            recoverable: true,
        }
    }

    fn fake_manifest(id: u128, version: u64) -> ManifestRef {
        ManifestRef {
            manifest_id: uuid::Uuid::from_u128(id),
            version,
        }
    }

    /// Helper: build a HostRegistry + register one target host with
    /// fresh capacity. Returns (registry, target_host, target_backend).
    fn build_registry_with_target(
        meta: Arc<FakeMeta>,
    ) -> (Arc<HostRegistry>, HostId, Arc<FakeBackend>) {
        let registry = Arc::new(HostRegistry::new(meta));
        let target_host = HostId::new();
        let target_be = Arc::new(FakeBackend::default());
        registry.register(target_host, target_be.clone());
        // pick_for_session's capacity fallback path requires a fresh
        // host with no draining flag — register() sets defaults that
        // suffice.
        (registry, target_host, target_be)
    }

    /// Arm 1: snapshot present + live_disk fresher (same lineage, higher
    /// version) → restore uses the live manifest as disk, snapshot's
    /// memory_manifest as memory. Loss=None.
    #[tokio::test]
    async fn evac_dead_source_uses_live_disk_when_newer() {
        let meta = Arc::new(FakeMeta::default());
        let lineage = 0xABCD;
        let mut session = make_session(HostId::new(), SandboxId::new(), SessionState::HostLost);
        session.live_disk_manifest = Some(fake_manifest(lineage, 5));
        let session_id = session.id;
        meta.install_session(session.clone());

        let snapshot = make_snapshot_for(
            session_id,
            Some(fake_manifest(lineage, 3)),
            Some(fake_manifest(lineage + 1, 1)),
        );

        let (registry, target_host, target_be) = build_registry_with_target(meta.clone());
        let new_sandbox = SandboxId::new();
        target_be.set_restore_id(new_sandbox);

        let receipt = evacuate_dead_source(
            &registry,
            &(meta.clone() as Arc<dyn MetadataStore>),
            session.clone(),
            Some(snapshot),
        )
        .await
        .expect("happy path");
        assert_eq!(receipt.new_host_id, target_host);
        assert_eq!(receipt.new_sandbox_id, new_sandbox);
        assert_eq!(receipt.loss, EvacLoss::None);

        let updated = meta.session(session_id);
        assert_eq!(updated.status, SessionState::Created);
        assert_eq!(updated.host_id, Some(target_host));
        assert_eq!(updated.sandbox_id, Some(new_sandbox));
    }

    /// Arm 2: snapshot present, no live_disk → restore uses the
    /// snapshot's disk + memory. Loss=None.
    #[tokio::test]
    async fn evac_dead_source_uses_snapshot_when_no_live_disk() {
        let meta = Arc::new(FakeMeta::default());
        let mut session = make_session(HostId::new(), SandboxId::new(), SessionState::HostLost);
        session.live_disk_manifest = None;
        let session_id = session.id;
        meta.install_session(session.clone());

        let snapshot = make_snapshot_for(
            session_id,
            Some(fake_manifest(0x1234, 7)),
            Some(fake_manifest(0x5678, 7)),
        );

        let (registry, _target_host, target_be) = build_registry_with_target(meta.clone());
        target_be.set_restore_id(SandboxId::new());

        let receipt = evacuate_dead_source(
            &registry,
            &(meta.clone() as Arc<dyn MetadataStore>),
            session.clone(),
            Some(snapshot),
        )
        .await
        .expect("snapshot-only happy path");
        assert_eq!(receipt.loss, EvacLoss::None);
    }

    /// Arm 3: no snapshot, live_disk present → disk-only restore.
    /// Loss=Memory{reason}.
    #[tokio::test]
    async fn evac_dead_source_disk_only_records_memory_loss() {
        let meta = Arc::new(FakeMeta::default());
        let mut session = make_session(HostId::new(), SandboxId::new(), SessionState::HostLost);
        session.live_disk_manifest = Some(fake_manifest(0xCAFE, 9));
        let session_id = session.id;
        meta.install_session(session.clone());

        let (registry, _target_host, target_be) = build_registry_with_target(meta.clone());
        target_be.set_restore_id(SandboxId::new());

        let receipt = evacuate_dead_source(
            &registry,
            &(meta.clone() as Arc<dyn MetadataStore>),
            session.clone(),
            None,
        )
        .await
        .expect("disk-only happy path");
        match &receipt.loss {
            EvacLoss::Memory { reason } => {
                assert_eq!(reason, "source-dead-no-snapshot");
            }
            other => panic!("expected Memory loss, got {other:?}"),
        }

        // PG was rebound through HostLost → Created.
        let updated = meta.session(session_id);
        assert_eq!(updated.status, SessionState::Created);
    }

    /// Arm 4: no snapshot, no live_disk → NoRecoverableState. Caller
    /// (dead_host.rs) routes this to HostLost → Dead.
    #[tokio::test]
    async fn evac_dead_source_no_state_errors_no_recoverable() {
        let meta = Arc::new(FakeMeta::default());
        let mut session = make_session(HostId::new(), SandboxId::new(), SessionState::HostLost);
        session.live_disk_manifest = None;
        let session_id = session.id;
        meta.install_session(session.clone());

        let (registry, _target_host, _target_be) = build_registry_with_target(meta.clone());

        let result = evacuate_dead_source(
            &registry,
            &(meta.clone() as Arc<dyn MetadataStore>),
            session.clone(),
            None,
        )
        .await;
        assert!(matches!(result, Err(EvacError::NoRecoverableState)));

        // PG untouched at HostLost.
        let updated = meta.session(session_id);
        assert_eq!(updated.status, SessionState::HostLost);
    }

    /// No registered target host → NoTargetAvailable. Caller logs +
    /// may retry.
    #[tokio::test]
    async fn evac_dead_source_no_target_returns_no_target_available() {
        let meta = Arc::new(FakeMeta::default());
        let mut session = make_session(HostId::new(), SandboxId::new(), SessionState::HostLost);
        session.live_disk_manifest = Some(fake_manifest(0x1, 1));
        meta.install_session(session.clone());

        // Empty registry — no hosts to pick.
        let registry = Arc::new(HostRegistry::new(meta.clone()));

        let result = evacuate_dead_source(
            &registry,
            &(meta.clone() as Arc<dyn MetadataStore>),
            session.clone(),
            None,
        )
        .await;
        assert!(matches!(result, Err(EvacError::NoTargetAvailable(_))));
    }

    /// pick_evac_disk_manifest semantics: same lineage → newer wins;
    /// different lineage → snapshot wins; None handling. Cheap pure-
    /// function test, mirrors the api/snapshot.rs effective_resume
    /// tests.
    #[test]
    fn pick_disk_prefers_live_only_when_newer_same_lineage() {
        let live = fake_manifest(0xAA, 5);
        let snap = fake_manifest(0xAA, 3);
        assert_eq!(pick_evac_disk_manifest(Some(live), Some(snap)), Some(live));
    }

    #[test]
    fn pick_disk_falls_back_to_snapshot_on_different_lineage() {
        let live = fake_manifest(0xAA, 99);
        let snap = fake_manifest(0xBB, 1);
        assert_eq!(pick_evac_disk_manifest(Some(live), Some(snap)), Some(snap),);
    }

    #[test]
    fn pick_disk_handles_either_none() {
        let snap = fake_manifest(0xAA, 1);
        assert_eq!(pick_evac_disk_manifest(None, Some(snap)), Some(snap));
        let live = fake_manifest(0xBB, 1);
        assert_eq!(pick_evac_disk_manifest(Some(live), None), Some(live));
        assert_eq!(pick_evac_disk_manifest(None, None), None);
    }
}
