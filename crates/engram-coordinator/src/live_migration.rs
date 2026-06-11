//! ADR 0045 C1: the live-teleport coordinator verb.
//!
//! `migrate_session_live` drives the eager dirty-set push synchronously
//! under the session lease: freeze the source (`migration_capture`, no
//! GCS on the pause path) → mark `Evacuating` (the crash parachute: if
//! this coordinator dies anywhere past here, the lease expires and the
//! existing evac scanner snapshot-rehomes from the last DURABLE
//! checkpoint row — rung-1 semantics with zero new recovery code) →
//! restore on the pinned destination (the dest pulls the export
//! host-to-host) → rebind → harness rebuild → commit the source → spawn
//! the durability catch-up finalize (row-only-at-finalize via the
//! dest's `snapshot_wait`).
//!
//! Failure arms: anything before the dest restore lands ⇒
//! `migration_abort` un-pauses the source in place (zero loss) and the
//! session returns to `Active`. A pre-C1 source/dest surfaces
//! `InvalidSpec` from capture ⇒ [`MigrateError::Unsupported`] and the
//! caller falls back to the snapshot-rehome teleport.

use engram_core::types::snapshot::MigrationSourceInfo;
use engram_core::types::SessionState;
use engram_core::SandboxError;
use engram_core::{HostId, SessionId};

use crate::idle_evictor::SessionLeaseGuard;
use crate::state::{SessionEvent, SharedState};

#[derive(Debug)]
pub enum MigrateError {
    /// Source or destination can't do a live move — fall back to
    /// snapshot-rehome (the pre-C1 teleport).
    Unsupported(String),
    /// The move failed but the source was aborted back to Active —
    /// downtime only, zero loss.
    AbortedToSource(String),
    /// The move failed in a state the parachute owns: the session is
    /// `Evacuating` and the scanner will rehome from the last durable
    /// checkpoint (loss ≤ one cadence).
    Parachute(String),
    Fatal(String),
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(m) => write!(f, "live migration unsupported: {m}"),
            Self::AbortedToSource(m) => write!(f, "live migration aborted to source: {m}"),
            Self::Parachute(m) => write!(f, "live migration failed; scanner rehome armed: {m}"),
            Self::Fatal(m) => write!(f, "live migration failed: {m}"),
        }
    }
}

/// Feature gate: `ENGRAM_LIVE_TELEPORT=1`. Off ⇒ the teleport verb keeps
/// the snapshot-rehome path unconditionally.
pub fn live_teleport_enabled() -> bool {
    std::env::var("ENGRAM_LIVE_TELEPORT")
        .map(|v| v == "1")
        .unwrap_or(false)
}

pub async fn migrate_session_live(
    state: &SharedState,
    session_id: SessionId,
    target_host_id: HostId,
) -> Result<(), MigrateError> {
    let t_total = std::time::Instant::now();
    // Serialize against resumes / evictions / sibling migrations.
    let session = state
        .services
        .meta
        .get_session(session_id)
        .await
        .map_err(|e| MigrateError::Fatal(format!("get_session: {e}")))?;
    if session.status != SessionState::Active {
        return Err(MigrateError::Fatal(format!(
            "live migration needs an Active session (got {})",
            session.status.as_str(),
        )));
    }
    let Some(sandbox_id) = session.sandbox_id else {
        return Err(MigrateError::Fatal("no bound sandbox".into()));
    };
    let lease = match SessionLeaseGuard::try_acquire(state, session_id, Some(sandbox_id)).await {
        Ok(Some(g)) => g,
        Ok(None) => {
            return Err(MigrateError::Fatal(
                "session is mid-resume/eviction/migration (lease held)".into(),
            ))
        }
        Err(e) => return Err(MigrateError::Fatal(format!("lease acquire: {e}"))),
    };

    // The destination must be takeable and the source addressable
    // before we freeze anything.
    let (_, dest_backend) = state
        .host_registry
        .pick_specific_host(target_host_id, session.host_id)
        .map_err(|e| MigrateError::Fatal(format!("target host can't take the session: {e:?}")))?;
    let source_addr = source_host_addr(state, session.host_id)
        .await
        .ok_or_else(|| {
            MigrateError::Unsupported("source host has no advertised host_addr".into())
        })?;

    // The last durable checkpoint row: the metadata template AND the
    // parachute's landing spot. OPTIONAL by operator decision
    // (2026-06-11): a session with no durable row yet (younger than
    // its first periodic checkpoint) teleports anyway — if the move
    // fails mid-flight there is nothing to rehome from and the
    // session is lost. Metadata falls back to the image's base
    // snapshot row (aux_bundles must still pin, or a [git] image
    // restores without its skills mounts).
    let durable_row = state
        .services
        .meta
        .latest_snapshot_for_session(session_id)
        .await
        .map_err(|e| MigrateError::Fatal(format!("latest snapshot: {e}")))?;
    let base_row = match &durable_row {
        Some(_) => None,
        None => match state
            .services
            .meta
            .get_enabled_image_any(&session.image)
            .await
        {
            Ok(Some(img)) => match img.base_snapshot_id {
                Some(id) => state.services.meta.get_snapshot(id).await.ok().flatten(),
                None => None,
            },
            _ => None,
        },
    };
    if durable_row.is_none() {
        tracing::warn!(
            %session_id,
            "live migration without a durable checkpoint row — a mid-move              failure past the freeze CANNOT be rehomed (operator-accepted)",
        );
    }

    // Resolve the source's backend handle ONCE, pre-freeze, and use it
    // for capture, the failure-arm aborts, AND the post-rebind commit.
    // Commit cannot route by sandbox id: step 4's
    // `invalidate_sandbox(sandbox_id)` drops the old route on purpose
    // (the old sandbox must stop serving exec), and the PG read-through
    // finds nothing because the session row already points at the new
    // sandbox — prod canary 5fa742b7 hit exactly this ("sandbox not
    // found" on commit; the source stayed frozen until the export TTL
    // destroyed it ~2 min later).
    let source_backend = match state.host_registry.resolve_owner(sandbox_id).await {
        Ok((_, backend)) => backend,
        Err(e) => return Err(MigrateError::Fatal(format!("resolve source host: {e}"))),
    };

    // ---- 1. Freeze the source (downtime clock starts) ----
    let t_capture = std::time::Instant::now();
    let capture = match source_backend.migration_capture(sandbox_id).await {
        Ok(c) => c,
        Err(SandboxError::InvalidSpec(reason)) => {
            return Err(MigrateError::Unsupported(reason));
        }
        Err(e) => return Err(MigrateError::Fatal(format!("migration capture: {e}"))),
    };
    let capture_ms = t_capture.elapsed().as_millis();

    // ---- 2. Arm the parachute ----
    // From here until the dest rebind, a coordinator death leaves the
    // session Evacuating: lease expiry → scanner snapshot-rehome from
    // `durable_row` (the capture's v+1 is never visible to it — it was
    // deliberately not published). The frozen source self-cleans via
    // the export TTL.
    if let Err(e) = state
        .services
        .meta
        .transition_session(session_id, SessionState::Evacuating)
        .await
    {
        let _ = source_backend
            .migration_abort(sandbox_id, &capture.export_id)
            .await;
        return Err(MigrateError::AbortedToSource(format!("transition: {e}")));
    }
    state.teleport_targets.insert(session_id, target_host_id);

    // ---- 3. Restore on the destination (pull + UFFD bring-up) ----
    let t_restore = std::time::Instant::now();
    let base_memory_manifest =
        crate::api::snapshot::base_memory_manifest_for_image(state, &session.image).await;
    let has_disk = !capture.disk_manifest_json.is_empty();
    let metadata = engram_core::types::snapshot::SnapshotMetadata {
        id: capture.snapshot_id,
        size_bytes: durable_row.as_ref().map(|r| r.size_bytes).unwrap_or(0),
        created_at: chrono::Utc::now(),
        image_version: durable_row
            .as_ref()
            .map(|r| r.image_version.clone())
            .or_else(|| base_row.as_ref().map(|r| r.image_version.clone()))
            .unwrap_or_else(|| {
                session
                    .image
                    .rsplit(':')
                    .next()
                    .unwrap_or("unknown")
                    .to_string()
            }),
        base_memory_manifest,
        migration_source: Some(MigrationSourceInfo {
            export_id: capture.export_id.clone(),
            source_addr,
            memory_manifest_json: capture.memory_manifest_json,
            disk_manifest_json: capture.disk_manifest_json,
            memory_manifest_ref: capture.memory_manifest_ref,
            disk_manifest_ref: capture.disk_manifest_ref,
            new_memory_chunk_hashes: capture.new_memory_chunk_hashes,
            new_disk_chunk_hashes: capture.new_disk_chunk_hashes,
        }),
        disk_manifest: has_disk.then_some(capture.disk_manifest_ref),
        memory_manifest: Some(capture.memory_manifest_ref),
        source_sandbox_id: None,
        state_blob_key: None,
        sidecar_blob_key: None,
        rootfs_blob_key: None,
        working_set_blob_key: None,
        aux_bundles: durable_row
            .as_ref()
            .map(|r| r.aux_bundles.clone())
            .or_else(|| base_row.as_ref().map(|r| r.aux_bundles.clone()))
            .unwrap_or_default(),
    };
    let new_sandbox_id = match dest_backend.restore(metadata).await {
        Ok(id) => id,
        Err(e) => {
            // Dest failure pre-rebind: un-pause the source in place and
            // walk the session back to Active. Zero loss.
            let abort_ok = source_backend
                .migration_abort(sandbox_id, &capture.export_id)
                .await
                .is_ok();
            state.teleport_targets.remove(&session_id);
            if abort_ok {
                let back = walk_back_to_active(state, session_id).await;
                if back {
                    return Err(MigrateError::AbortedToSource(format!("dest restore: {e}")));
                }
            }
            return Err(MigrateError::Parachute(format!("dest restore: {e}")));
        }
    };
    let restore_ms = t_restore.elapsed().as_millis();

    // ---- 4. Rebind + reactivate (mirrors the evacuation tail) ----
    state.host_registry.invalidate_sandbox(sandbox_id);
    state
        .host_registry
        .record_sandbox_owner(new_sandbox_id, target_host_id);
    let rebind = async {
        state
            .services
            .meta
            .assign_session_host(session_id, Some(target_host_id))
            .await
            .map_err(|e| format!("assign host: {e}"))?;
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(new_sandbox_id))
            .await
            .map_err(|e| format!("assign sandbox: {e}"))?;
        state
            .services
            .meta
            .transition_session(session_id, SessionState::Created)
            .await
            .map_err(|e| format!("to Created: {e}"))?;
        Ok::<(), String>(())
    }
    .await;
    if let Err(e) = rebind {
        // The dest VM exists but PG didn't take the rebind — leave the
        // parachute armed (scanner rehome); the dest orphan falls to
        // orphan_reap, the frozen source to the export TTL.
        let _ = dest_backend.destroy(new_sandbox_id).await;
        state.teleport_targets.remove(&session_id);
        return Err(MigrateError::Parachute(e));
    }
    crate::api::snapshot::bind_session_routing(state, session_id, new_sandbox_id).await;
    let session_refreshed = state
        .services
        .meta
        .get_session(session_id)
        .await
        .map_err(|e| MigrateError::Parachute(format!("refresh session: {e}")))?;
    if let Err(e) =
        crate::api::snapshot::finish_resume_to_active(state, &session_refreshed, new_sandbox_id)
            .await
    {
        // Harness rebuild failed; session sits at Created — the same
        // posture the evac resumer leaves on this failure. The move
        // itself landed; don't abort the source back.
        tracing::warn!(%session_id, error = %e,
            "live migration: finish_resume_to_active failed; session left at Created");
    }
    state.teleport_targets.remove(&session_id);

    // ---- 5. Commit the source + finalize durability in background ----
    // Via the pre-freeze handle: the registry can no longer route the
    // old sandbox id (invalidated at step 4, and PG points at the new
    // sandbox), so sandbox-routed dispatch would land "sandbox not
    // found" and leave the source frozen until the export TTL.
    if let Err(e) = source_backend
        .migration_commit(sandbox_id, &capture.export_id)
        .await
    {
        tracing::warn!(%session_id, %sandbox_id, error = %e,
            "live migration: source commit failed; export TTL will clean up");
    }
    let _ = state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: SessionState::Evacuating,
                to: SessionState::Active,
                at: chrono::Utc::now(),
            },
        )
        .await;
    metrics::histogram!(crate::metrics::MIGRATION_LEG_SECONDS, "leg" => "capture")
        .record(capture_ms as f64 / 1000.0);
    metrics::histogram!(crate::metrics::MIGRATION_LEG_SECONDS, "leg" => "restore")
        .record(restore_ms as f64 / 1000.0);
    metrics::histogram!(crate::metrics::MIGRATION_LEG_SECONDS, "leg" => "total")
        .record(t_total.elapsed().as_secs_f64());
    metrics::counter!(crate::metrics::MIGRATION_TOTAL, "outcome" => "migrated").increment(1);
    tracing::info!(
        %session_id,
        old_sandbox = %sandbox_id,
        new_sandbox = %new_sandbox_id,
        target_host = %target_host_id,
        capture_ms,
        restore_ms,
        total_ms = t_total.elapsed().as_millis(),
        "live teleport complete (ADR 0045 C1); durability catch-up finalizing",
    );

    // Row-only-at-finalize (the D5 pattern): the dest's catch-up makes
    // the v+1 manifests + chunks durable, then the row lands. Failure ⇒
    // no row; the previous checkpoint stays the fallback and the dest's
    // chain was dropped host-side (next checkpoint goes Full).
    let state2 = state.clone();
    let row_at = chrono::Utc::now();
    tokio::spawn(async move {
        let lease = lease; // held + touched until the row lands
        let mut touch = tokio::time::interval(std::time::Duration::from_secs(60));
        touch.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        touch.tick().await;
        let wait = state2.services.host.snapshot_wait(new_sandbox_id);
        tokio::pin!(wait);
        let row_meta = loop {
            tokio::select! {
                res = &mut wait => break res,
                _ = touch.tick() => {
                    if !lease.touch().await {
                        tracing::error!(%session_id, "live migration finalize: lease lost; no row");
                        return;
                    }
                }
            }
        };
        let row_meta = match row_meta {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(%session_id, error = %e,
                    "live migration finalize: catch-up failed; previous checkpoint stays the fallback");
                return;
            }
        };
        let record = engram_core::types::snapshot::SnapshotRecord {
            id: row_meta.id,
            session_id: Some(session_id),
            host_id: Some(target_host_id),
            image_version: row_meta.image_version.clone(),
            size_bytes: row_meta.size_bytes,
            created_at: row_meta.created_at,
            last_accessed_at: row_at,
            disk_manifest: row_meta.disk_manifest,
            memory_manifest: row_meta.memory_manifest,
            recoverable: crate::api::snapshot::verify_snapshot_recoverable(
                state2.services.blob.as_ref(),
                row_meta.disk_manifest.as_ref(),
                row_meta.memory_manifest.as_ref(),
            )
            .await,
            aux_bundles: row_meta.aux_bundles.clone(),
            events_cursor: None,
        };
        if let Err(e) = state2.services.meta.record_snapshot(record).await {
            tracing::warn!(%session_id, error = %e, "live migration finalize: record_snapshot failed");
            return;
        }
        tracing::info!(%session_id, snapshot_id = %row_meta.id,
            "live migration durability finalized (row-only-at-finalize)");
    });
    Ok(())
}

/// The source host-agent's gRPC address. The heartbeat-warmed pool is
/// authoritative — `hosts.host_addr` in PG is written only at REGISTER,
/// and a host-agent pod that reattaches after a roll doesn't
/// re-register, leaving the PG row pointing at the PREVIOUS pod
/// generation (the prod canary's dest dialed a dead pod-network IP
/// exactly this way). PG is the fallback for a host the pool hasn't
/// warmed since the coordinator's own restart.
async fn source_host_addr(state: &SharedState, host_id: Option<HostId>) -> Option<String> {
    let host_id = host_id?;
    if let Some(addr) = state.services.host_pool.current_addr(host_id) {
        return Some(addr);
    }
    let hosts = state.services.meta.list_active_hosts().await.ok()?;
    hosts.into_iter().find(|h| h.id == host_id)?.host_addr
}

/// Evacuating → Created → Active without relocating (the source VM was
/// aborted back in place; its bindings never changed). Returns false if
/// either transition is refused (the parachute then owns recovery).
async fn walk_back_to_active(state: &SharedState, session_id: SessionId) -> bool {
    for target in [SessionState::Created, SessionState::Active] {
        if let Err(e) = state
            .services
            .meta
            .transition_session(session_id, target)
            .await
        {
            tracing::warn!(%session_id, ?target, error = %e,
                "live migration walk-back transition refused");
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_cloud_mock::MockCloud;
    use engram_core::traits::MetadataStore;
    use engram_core::types::session::{Session, SessionMode};
    use engram_core::SandboxId;
    use std::sync::Arc;

    fn active_session() -> Session {
        Session {
            id: SessionId::new(),
            user_id: None,
            status: SessionState::Active,
            host_id: Some(HostId::new()),
            sandbox_id: Some(SandboxId::new()),
            image: "test/repo:live-migrate".into(),
            mode: SessionMode::Agent,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
            live_disk_manifest: None,
        }
    }

    fn build_state(session: Session) -> (SharedState, Arc<MiniMeta>, HostId) {
        let tmp = std::env::temp_dir().join(format!("live-migrate-test-{}", session.id));
        std::fs::create_dir_all(&tmp).unwrap();
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        // A registered, ready target host (ProcessBackend-backed local
        // client: every migration_* trait method is the default
        // InvalidSpec — the pre-C1 host shape).
        let target = HostId::new();
        let backend: Arc<dyn engram_core::traits::SandboxBackend> = Arc::new(
            engram_sandbox_process::ProcessBackend::new(tmp.join("sandboxes")),
        );
        host_registry.register(
            target,
            Arc::new(engram_host_agent::host_client::LocalHostClient::with_noop_hub(backend)),
        );
        host_registry.update_state(
            target,
            crate::host_registry::HostState {
                capacity: engram_protocol::heartbeat::HostCapacityReport {
                    total_mib: 16_384,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                local_snapshots: Vec::new(),
                draining: false,
                ready_images: Default::default(),
                current_bundles: Vec::new(),
                utilization: Default::default(),
            },
        );
        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            secrets: Arc::new(engram_secrets_dev::InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                tmp.join("blobs"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(tmp.join("blobs")),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
        };
        let cfg = CoordinatorConfig {
            local_path: tmp,
            ..CoordinatorConfig::default()
        };
        (
            Arc::new(AppState::new_with_registry(cfg, services, host_registry)),
            meta,
            target,
        )
    }

    /// The fallback contract: a fleet that can't do a live move (no
    /// durable row / no host_addr / pre-C1 capture) yields
    /// `Unsupported` — never a frozen guest, never a state change —
    /// and the caller falls back to snapshot-rehome. The session must
    /// be left EXACTLY as found, lease released.
    #[tokio::test]
    async fn unsupported_fleet_falls_back_without_touching_the_session() {
        let session = active_session();
        let session_id = session.id;
        let (state, meta, target) = build_state(session);

        let err = migrate_session_live(&state, session_id, target)
            .await
            .expect_err("pre-C1 fleet must be unsupported");
        assert!(
            matches!(err, MigrateError::Unsupported(_)),
            "got {err:?} — only Unsupported triggers the snapshot-rehome fallback",
        );
        let after = meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::Active, "session untouched");
        // Lease released (Drop spawns a detached DELETE — poll briefly).
        let mut released = false;
        for _ in 0..40 {
            if meta
                .try_acquire_session_lease(session_id, None, "follow-up")
                .await
                .unwrap()
            {
                released = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(released, "lease must release after the fallback");
    }

    /// The dest-failure arm: capture succeeds (the source is frozen),
    /// the destination restore fails — the verb ABORTS the source back
    /// (un-pause in place) and walks the session to Active. Zero loss,
    /// AbortedToSource posture.
    #[tokio::test]
    async fn dest_restore_failure_aborts_to_source_and_walks_back_to_active() {
        use engram_core::types::snapshot::{MigrationCaptureOut, SnapshotMetadata};
        use std::sync::atomic::{AtomicBool, Ordering};

        struct MigratableFlakyDest {
            abort_called: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl engram_core::traits::SandboxBackend for MigratableFlakyDest {
            async fn create(
                &self,
                _: engram_core::types::sandbox::SandboxSpec,
            ) -> Result<SandboxId, engram_core::SandboxError> {
                Ok(SandboxId::new())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), engram_core::SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, engram_core::SandboxError> {
                Ok(Vec::new())
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                Err(engram_core::SandboxError::NotFound)
            }
            async fn snapshot(
                &self,
                _: SandboxId,
            ) -> Result<SnapshotMetadata, engram_core::SandboxError> {
                Err(engram_core::SandboxError::NotFound)
            }
            async fn migration_capture(
                &self,
                _: SandboxId,
            ) -> Result<MigrationCaptureOut, engram_core::SandboxError> {
                let mref = engram_core::types::manifest::ManifestRef::new();
                Ok(MigrationCaptureOut {
                    export_id: "test-export".into(),
                    memory_manifest_json: b"{}".to_vec(),
                    disk_manifest_json: Vec::new(),
                    memory_manifest_ref: mref,
                    disk_manifest_ref: engram_core::types::manifest::ManifestRef::new(),
                    new_memory_chunk_hashes: Vec::new(),
                    new_disk_chunk_hashes: Vec::new(),
                    snapshot_id: engram_core::types::SnapshotId::new(),
                    paused_at_unix_ms: 0,
                })
            }
            async fn migration_abort(
                &self,
                _: SandboxId,
                _: &str,
            ) -> Result<(), engram_core::SandboxError> {
                self.abort_called.store(true, Ordering::SeqCst);
                Ok(())
            }
            async fn restore(
                &self,
                _: SnapshotMetadata,
            ) -> Result<SandboxId, engram_core::SandboxError> {
                Err(engram_core::SandboxError::Snapshot(
                    "injected dest failure".into(),
                ))
            }
            fn snapshot_path_for(&self, _: engram_core::types::SnapshotId) -> std::path::PathBuf {
                std::path::PathBuf::from("/nonexistent")
            }
        }

        let session = active_session();
        let session_id = session.id;
        let source_host = session.host_id.expect("source host");
        let (state, meta, target) = build_state(session);
        // Replace the registry's target backend with the migratable
        // flaky one — same host id, capture-capable, restore-failing.
        let abort_called = Arc::new(AtomicBool::new(false));
        let flaky: Arc<dyn engram_core::traits::SandboxBackend> = Arc::new(MigratableFlakyDest {
            abort_called: abort_called.clone(),
        });
        state.host_registry.register(
            target,
            Arc::new(engram_host_agent::host_client::LocalHostClient::with_noop_hub(flaky.clone())),
        );
        // Re-register resets the heartbeat state — restore capacity.
        state.host_registry.update_state(
            target,
            crate::host_registry::HostState {
                capacity: engram_protocol::heartbeat::HostCapacityReport {
                    total_mib: 16_384,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                local_snapshots: Vec::new(),
                draining: false,
                ready_images: Default::default(),
                current_bundles: Vec::new(),
                utilization: Default::default(),
            },
        );
        // The SOURCE is resolved through services.host (the registry) by
        // sandbox owner — record the source sandbox's owner as the same
        // flaky backend (it serves capture + abort).
        state
            .host_registry
            .record_sandbox_owner(meta.session.lock().sandbox_id.unwrap(), target);
        // Source host row with an addr + a durable checkpoint row.
        meta.hosts
            .lock()
            .push(engram_core::types::host::HostRecord {
                id: source_host,
                hostname: "src".into(),
                cloud_metadata: engram_core::types::host::HostMetadata::default(),
                capacity: engram_core::types::host::HostCapacity {
                    total_gb: 100,
                    used_gb: 10,
                    total_mib: 65_536,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                utilization: Default::default(),
                status: engram_core::types::host::HostStatus::Ready,
                last_heartbeat_at: chrono::Utc::now(),
                host_addr: Some("http://127.0.0.1:1".into()),
            });
        meta.snapshots
            .lock()
            .push(engram_core::types::snapshot::SnapshotRecord {
                id: engram_core::types::SnapshotId::new(),
                session_id: Some(session_id),
                host_id: Some(source_host),
                image_version: "test".into(),
                size_bytes: 1,
                created_at: chrono::Utc::now(),
                last_accessed_at: chrono::Utc::now(),
                disk_manifest: None,
                memory_manifest: Some(engram_core::types::manifest::ManifestRef::new()),
                recoverable: true,
                aux_bundles: Vec::new(),
                events_cursor: None,
            });

        let err = migrate_session_live(&state, session_id, target)
            .await
            .expect_err("dest failure must surface");
        assert!(
            matches!(err, MigrateError::AbortedToSource(_)),
            "got {err:?}",
        );
        assert!(
            abort_called.load(Ordering::SeqCst),
            "source must be aborted"
        );
        assert_eq!(
            meta.get_session(session_id).await.unwrap().status,
            SessionState::Active,
            "session walks back to Active (zero loss)",
        );
    }

    /// The commit-routing regression (prod canary 5fa742b7): step 4's
    /// `invalidate_sandbox` + the PG rebind make the OLD sandbox id
    /// unroutable, so a sandbox-routed `migration_commit` lands
    /// "sandbox not found" and the frozen source lingers until the
    /// export TTL. The verb must commit through the source backend
    /// handle it resolved before freezing.
    #[tokio::test]
    async fn commit_reaches_the_frozen_source_after_rebind() {
        use engram_core::types::snapshot::{MigrationCaptureOut, SnapshotMetadata};
        use std::sync::atomic::{AtomicBool, Ordering};

        struct MigratableHappyPath {
            commit_called: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl engram_core::traits::SandboxBackend for MigratableHappyPath {
            async fn create(
                &self,
                _: engram_core::types::sandbox::SandboxSpec,
            ) -> Result<SandboxId, engram_core::SandboxError> {
                Ok(SandboxId::new())
            }
            async fn destroy(&self, _: SandboxId) -> Result<(), engram_core::SandboxError> {
                Ok(())
            }
            async fn list(&self) -> Result<Vec<SandboxId>, engram_core::SandboxError> {
                Ok(Vec::new())
            }
            async fn exec_stream(
                &self,
                _: SandboxId,
                _: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                Err(engram_core::SandboxError::NotFound)
            }
            async fn snapshot(
                &self,
                _: SandboxId,
            ) -> Result<SnapshotMetadata, engram_core::SandboxError> {
                Err(engram_core::SandboxError::NotFound)
            }
            async fn migration_capture(
                &self,
                _: SandboxId,
            ) -> Result<MigrationCaptureOut, engram_core::SandboxError> {
                let mref = engram_core::types::manifest::ManifestRef::new();
                Ok(MigrationCaptureOut {
                    export_id: "test-export".into(),
                    memory_manifest_json: b"{}".to_vec(),
                    disk_manifest_json: Vec::new(),
                    memory_manifest_ref: mref,
                    disk_manifest_ref: engram_core::types::manifest::ManifestRef::new(),
                    new_memory_chunk_hashes: Vec::new(),
                    new_disk_chunk_hashes: Vec::new(),
                    snapshot_id: engram_core::types::SnapshotId::new(),
                    paused_at_unix_ms: 0,
                })
            }
            async fn migration_commit(
                &self,
                _: SandboxId,
                _: &str,
            ) -> Result<(), engram_core::SandboxError> {
                self.commit_called.store(true, Ordering::SeqCst);
                Ok(())
            }
            async fn restore(
                &self,
                _: SnapshotMetadata,
            ) -> Result<SandboxId, engram_core::SandboxError> {
                Ok(SandboxId::new())
            }
            fn snapshot_path_for(&self, _: engram_core::types::SnapshotId) -> std::path::PathBuf {
                std::path::PathBuf::from("/nonexistent")
            }
        }

        let session = active_session();
        let session_id = session.id;
        let source_host = session.host_id.expect("source host");
        let old_sandbox = session.sandbox_id.expect("source sandbox");
        let (state, meta, target) = build_state(session);
        let commit_called = Arc::new(AtomicBool::new(false));
        let happy: Arc<dyn engram_core::traits::SandboxBackend> = Arc::new(MigratableHappyPath {
            commit_called: commit_called.clone(),
        });
        state.host_registry.register(
            target,
            Arc::new(engram_host_agent::host_client::LocalHostClient::with_noop_hub(happy.clone())),
        );
        // Re-register resets the heartbeat state — restore capacity.
        state.host_registry.update_state(
            target,
            crate::host_registry::HostState {
                capacity: engram_protocol::heartbeat::HostCapacityReport {
                    total_mib: 16_384,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                local_snapshots: Vec::new(),
                draining: false,
                ready_images: Default::default(),
                current_bundles: Vec::new(),
                utilization: Default::default(),
            },
        );
        // The source resolves through the recorded sandbox owner (the
        // same fake backend serves both roles, as in the abort test).
        state
            .host_registry
            .record_sandbox_owner(old_sandbox, target);
        meta.hosts
            .lock()
            .push(engram_core::types::host::HostRecord {
                id: source_host,
                hostname: "src".into(),
                cloud_metadata: engram_core::types::host::HostMetadata::default(),
                capacity: engram_core::types::host::HostCapacity {
                    total_gb: 100,
                    used_gb: 10,
                    total_mib: 65_536,
                    used_mib: 0,
                    running_sandboxes: 0,
                },
                utilization: Default::default(),
                status: engram_core::types::host::HostStatus::Ready,
                last_heartbeat_at: chrono::Utc::now(),
                host_addr: Some("http://127.0.0.1:1".into()),
            });
        meta.snapshots
            .lock()
            .push(engram_core::types::snapshot::SnapshotRecord {
                id: engram_core::types::SnapshotId::new(),
                session_id: Some(session_id),
                host_id: Some(source_host),
                image_version: "test".into(),
                size_bytes: 1,
                created_at: chrono::Utc::now(),
                last_accessed_at: chrono::Utc::now(),
                disk_manifest: None,
                memory_manifest: Some(engram_core::types::manifest::ManifestRef::new()),
                recoverable: true,
                aux_bundles: Vec::new(),
                events_cursor: None,
            });

        migrate_session_live(&state, session_id, target)
            .await
            .expect("happy-path migration must succeed");
        assert!(
            commit_called.load(Ordering::SeqCst),
            "the frozen source must receive migration_commit even though \
             the old sandbox id is unroutable after the rebind",
        );
        let after = meta.get_session(session_id).await.unwrap();
        assert_eq!(after.host_id, Some(target), "session rebound to dest");
        assert_ne!(
            after.sandbox_id,
            Some(old_sandbox),
            "session points at the new sandbox",
        );
    }

    /// A held lease refuses the migration outright (Fatal, not a
    /// fallback — the rival owns the session right now).
    #[tokio::test]
    async fn held_lease_refuses_migration() {
        let session = active_session();
        let session_id = session.id;
        let (state, meta, target) = build_state(session);
        assert!(meta
            .try_acquire_session_lease(session_id, None, "rival")
            .await
            .unwrap());
        let err = migrate_session_live(&state, session_id, target)
            .await
            .expect_err("held lease must refuse");
        assert!(matches!(err, MigrateError::Fatal(_)));
    }
}
