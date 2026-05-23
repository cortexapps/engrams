//! Idle-session eviction *pipeline* — the snapshot+destroy+mark-Idle
//! primitive that runs when a sandbox has been quiet past its TTL.
//!
//! ADR 0013 + ADR 0011 follow-up #2 retired the polling driver that
//! lived here. In a stateless coord, no single pod's local
//! `HarnessHub` is authoritative for "is this sandbox idle?" — the
//! host owns that view. The host now scans its local hub on a tick
//! and POSTs candidates to `/api/hosts/:id/idle-eviction-candidates`;
//! the receiving coord pod runs `evict_idle_session` on each. The
//! pipeline is idempotent (registry guard at the top short-circuits
//! if another pod already evicted the sandbox), so the same
//! candidate landing twice is safe.
//!
//! Auto-resume on next request is wired separately (`api/sessions.rs`
//! exec/exec_stream/SSE handlers): if status is `Idle`, call the
//! existing `resume` path before routing.

use std::sync::Arc;

use chrono::Utc;
use engram_core::traits::SandboxBackend;
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::SessionState;
use engram_core::{SandboxId, SessionId};

use crate::state::{SessionEvent, SharedState};

/// Run the suspend pipeline for one sandbox. Pure function over
/// `SharedState`; the loop above is just the driver. Multi-host
/// production refactors the driver onto each host-agent and keeps
/// this function as the canonical pipeline.
pub async fn evict_idle_session(
    state: &SharedState,
    session_id: SessionId,
    sandbox_id: SandboxId,
) -> Result<(), EvictError> {
    // ADR 0016 A.1.1: entry log. Was silent before — a coord pod
    // running the pipeline repeatedly (e.g. retry storm, post-roll
    // race) showed up only as host-side `chunked NBD disk flushed`
    // lines with no coord-side counterpart, making the snapshot
    // source impossible to attribute. Pair this with the
    // "completed"/"skipping" logs below and the per-step warn arms
    // so the full pipeline is auditable end-to-end.
    tracing::info!(
        session_id = %session_id,
        sandbox_id = %sandbox_id,
        "idle eviction pipeline started",
    );

    // Guard: if the registry doesn't think this sandbox is bound to
    // the session anymore, the session was already evicted by some
    // other path (operator, dead-host detector). No-op cleanly.
    if state.registry.get(session_id) != Some(sandbox_id) {
        tracing::info!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            "idle eviction skipped: sandbox no longer bound",
        );
        return Ok(());
    }

    // Step 1: take a snapshot. ADR 0007 Phase 6: backend owns its
    // staging dir; coord no longer pre-allocates one. Durability
    // flows through the chunked manifests on `SnapshotMetadata`.
    //
    // ADR 0014 issue #1/#2: the host tracks this as an in-flight
    // snapshot. Every exit path after this point MUST end with either
    // `commit_snapshot` (full pipeline succeeded) or `abort_snapshot`
    // (anything else). Without this, a coord-side flake leaves the
    // 4 GiB local snapshot dir + per-snapshot blob keys orphaned —
    // host-side `idle_evictor` re-POSTs the candidate on the next tick,
    // a fresh SnapshotId is minted, and we leak ~4 GiB per retry. That
    // was the failure on `engrams-fc-xngk` (25 dirs × 4 GiB in 13 min).
    let metadata = state
        .services
        .host
        .snapshot(sandbox_id)
        .await
        .map_err(EvictError::Sandbox)?;

    let host_id = state.host_registry.host_of(sandbox_id);
    let now = Utc::now();
    let record = SnapshotRecord {
        id: metadata.id,
        session_id: Some(session_id),
        host_id,
        image_version: metadata.image_version.clone(),
        size_bytes: metadata.size_bytes,
        created_at: metadata.created_at,
        last_accessed_at: now,
        // ADR 0007: chunked manifests are the durability primitive.
        disk_manifest: metadata.disk_manifest,
        memory_manifest: metadata.memory_manifest,
        // ADR 0009 Phase 2: HEAD-verify the chunked manifests so
        // reconcile flips this session to Idle (not Dead) on a
        // future sandbox-loss event.
        recoverable: crate::api::snapshot::verify_snapshot_recoverable(
            state.services.blob.as_ref(),
            metadata.disk_manifest.as_ref(),
            metadata.memory_manifest.as_ref(),
        )
        .await,
    };
    if let Err(e) = state.services.meta.record_snapshot(record.clone()).await {
        abort_inflight_snapshot(state, session_id, sandbox_id, "record_snapshot").await;
        return Err(EvictError::Meta(e.to_string()));
    }

    // Step 3: destroy the sandbox. Best-effort — even on failure we
    // still want to mark the session Idle so a future resume doesn't
    // try to route to a dead sandbox.
    state.registry.unbind(session_id);
    if let Err(e) = state.services.host.destroy(sandbox_id).await {
        tracing::warn!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            error = %e,
            "idle eviction: destroy failed; continuing to mark session Idle",
        );
    }
    // ADR 0006: host-agent unregisters its local proxy entry as
    // part of `destroy`. No coordinator-side cleanup needed.

    // Step 4: clear sandbox_id, set Idle, emit events.
    if let Err(e) = state
        .services
        .meta
        .assign_session_sandbox(session_id, None)
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "idle eviction: assign_session_sandbox(None) failed",
        );
    }
    let prev = match state
        .services
        .meta
        .transition_session(session_id, SessionState::Idle)
        .await
    {
        Ok(prev) => prev,
        Err(e) => {
            abort_inflight_snapshot(state, session_id, sandbox_id, "transition_session").await;
            return Err(EvictError::Meta(e.to_string()));
        }
    };

    if let Err(e) = state
        .emit(
            session_id,
            SessionEvent::SnapshotTaken {
                snapshot_id: metadata.id,
                size_bytes: metadata.size_bytes,
                at: now,
            },
        )
        .await
    {
        abort_inflight_snapshot(state, session_id, sandbox_id, "emit SnapshotTaken").await;
        return Err(EvictError::Emit(e.to_string()));
    }
    if let Err(e) = state
        .emit(session_id, SessionEvent::Evicted { at: now })
        .await
    {
        abort_inflight_snapshot(state, session_id, sandbox_id, "emit Evicted").await;
        return Err(EvictError::Emit(e.to_string()));
    }
    if let Err(e) = state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: prev,
                to: SessionState::Idle,
                at: now,
            },
        )
        .await
    {
        abort_inflight_snapshot(state, session_id, sandbox_id, "emit StatusChanged").await;
        return Err(EvictError::Emit(e.to_string()));
    }

    // Full pipeline succeeded — commit the snapshot. Failure here is
    // unusual (host RPC error) and means the host's in-flight tracking
    // wasn't cleared, but the artifacts are still consistent. Log and
    // let it ride; the next snapshot() for this sandbox will overwrite
    // anyway, and the PG row still references the durable artifacts.
    if let Err(e) = state.services.host.commit_snapshot(sandbox_id).await {
        tracing::warn!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            error = %e,
            "idle eviction: commit_snapshot failed after full pipeline success \
             (host-side in-flight tracking may be stale until next retry overwrites)",
        );
    }

    // ADR 0016 A.1.1: success log. Pairs with the entry log so a
    // pipeline that flushes (host log) without committing (no PG
    // row) shows up as an unmatched start/end pair in a grep.
    tracing::info!(
        session_id = %session_id,
        sandbox_id = %sandbox_id,
        snapshot_id = %metadata.id,
        "idle eviction pipeline completed",
    );

    Ok(())
}

/// ADR 0014 issue #1/#2: best-effort `abort_snapshot` after a
/// downstream pipeline failure in [`evict_idle_session`]. Logs but
/// never propagates — the caller's pipeline error is what surfaces.
/// Hosts implement abort idempotently so spurious double-calls are
/// safe.
async fn abort_inflight_snapshot(
    state: &SharedState,
    session_id: SessionId,
    sandbox_id: SandboxId,
    scope: &'static str,
) {
    if let Err(e) = state.services.host.abort_snapshot(sandbox_id).await {
        tracing::warn!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            scope,
            error = %e,
            "idle eviction: abort_snapshot failed after pipeline failure \
             (orphan local dir + blobs may persist until next retry)",
        );
    }
}

#[derive(Debug)]
pub enum EvictError {
    Io(String),
    Sandbox(engram_core::SandboxError),
    Meta(String),
    Emit(String),
}

impl std::fmt::Display for EvictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(m) => write!(f, "idle evict io: {m}"),
            Self::Sandbox(e) => write!(f, "idle evict sandbox: {e}"),
            Self::Meta(m) => write!(f, "idle evict meta: {m}"),
            Self::Emit(m) => write!(f, "idle evict event emit: {m}"),
        }
    }
}

impl std::error::Error for EvictError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sandbox(e) => Some(e),
            _ => None,
        }
    }
}

// The TTL env helpers + polling driver live on the host-agent now
// (`engram_host_agent::idle_evictor`). The coord only owns the
// `evict_idle_session` pipeline above, invoked by the
// `/api/hosts/:id/idle-eviction-candidates` POST handler.

/// Marker that this module exists so unused-arg checkers don't
/// flag the `Arc<dyn SandboxBackend>` we explicitly take below.
#[allow(dead_code)]
fn _backend_unused_check<B: SandboxBackend>(_: Arc<B>) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CoordinatorConfig;
    use crate::host_registry::HostRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_cloud_mock::MockCloud;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
    use engram_core::types::session::HarnessSpec;
    use engram_core::types::{Session, SessionState};
    use engram_sandbox_process::ProcessBackend;
    use engram_secrets_dev::InMemorySecretStore;
    use std::path::Path;
    use std::time::Duration;
    use tempfile::TempDir;

    fn build_state_with_session(session: Session, sandbox_root: &Path) -> SharedState {
        let local_path = sandbox_root.join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );
        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: std::sync::Arc::new(engram_oci::OciClient::new(std::sync::Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: std::sync::Arc::new(engram_oci::AnonymousResolver),
            blob: std::sync::Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-blobs-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(std::sync::Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-blobs-test"),
                ),
            )),
            host_pool: std::sync::Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
        };
        let cfg = CoordinatorConfig {
            local_path,
            ..CoordinatorConfig::default()
        };
        Arc::new(AppState::new_with_registry(cfg, services, host_registry))
    }

    fn process_spec() -> SandboxSpec {
        SandboxSpec {
            image: "evict-test".into(),
            rootfs_source: None,
            image_uri: None,
            harness_pack_uri: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 256 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            harness_substrate: None,
            network: Default::default(),
        }
    }

    #[tokio::test]
    async fn evict_idle_session_runs_full_pipeline() {
        // ADR 0005: the eviction pipeline is now snapshot + destroy +
        // mark-Idle only. The auto-checkpoint pre-step is gone — git
        // is no longer the platform's durability primitive.
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-test".into(),
            harness: HarnessSpec::None,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
        };

        let sandbox_root = TempDir::new().unwrap();
        let state = build_state_with_session(session, sandbox_root.path());

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state.registry.bind(session_id, sandbox_id);
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        let req = ExecRequest {
            command: vec!["sh".into(), "-c".into(), "echo agent > out.txt".into()],
            stdin: None,
            env: Default::default(),
            workdir: None,
            timeout: Some(Duration::from_secs(5)),
        };
        state.services.host.exec(sandbox_id, req).await.unwrap();

        let mut sub = state.events.subscribe(session_id);

        evict_idle_session(&state, session_id, sandbox_id)
            .await
            .expect("eviction should succeed");

        // Session is Idle, sandbox_id cleared, registry unbound.
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::Idle);
        assert_eq!(after.sandbox_id, None);
        assert_eq!(state.registry.get(session_id), None);

        // A snapshot was recorded.
        let snaps = state
            .services
            .meta
            .list_snapshots_for_session(session_id)
            .await
            .unwrap();
        assert_eq!(snaps.len(), 1, "exactly one snapshot recorded");

        // Drain the bus and confirm the post-checkpoint sequence:
        // SnapshotTaken / Evicted / StatusChanged.
        let mut kinds: Vec<String> = Vec::new();
        while let Ok(ev) = tokio::time::timeout(Duration::from_millis(50), sub.recv()).await {
            if let Ok(indexed) = ev {
                kinds.push(indexed.event.kind().to_string());
            }
        }
        for required in ["snapshot_taken", "evicted", "status_changed"] {
            assert!(
                kinds.iter().any(|k| k == required),
                "missing event {required} in {kinds:?}"
            );
        }
        assert!(
            !kinds.iter().any(|k| k == "checkpoint_pushed"),
            "ADR 0005: checkpoint_pushed must no longer be emitted (got {kinds:?})"
        );
    }

    /// ADR 0014 issue #1/#2 regression guard. When `record_snapshot`
    /// fails post-snapshot, the pipeline MUST call
    /// `host.abort_snapshot(sandbox_id)` before bubbling — without
    /// this, the host leaks the per-snapshot dir (4 GiB on FC), which
    /// is exactly the failure mode that filled `engrams-fc-xngk` in
    /// 13 minutes.
    #[tokio::test]
    async fn evict_idle_session_aborts_snapshot_when_record_snapshot_fails() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc as StdArc;

        // Spy that wraps the real HostRegistry and counts
        // commit_snapshot + abort_snapshot calls.
        struct SpyHost {
            inner: StdArc<dyn engram_core::traits::HostClient>,
            aborts: StdArc<AtomicU32>,
            commits: StdArc<AtomicU32>,
        }

        #[async_trait::async_trait]
        impl engram_core::traits::HostClient for SpyHost {
            // Pass through all required methods to inner.
            async fn create(
                &self,
                spec: SandboxSpec,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.create(spec).await
            }
            async fn destroy(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.destroy(id).await
            }
            async fn list(&self) -> Result<Vec<engram_core::SandboxId>, engram_core::SandboxError> {
                self.inner.list().await
            }
            async fn exec_stream(
                &self,
                id: engram_core::SandboxId,
                cmd: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                self.inner.exec_stream(id, cmd).await
            }
            async fn snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<engram_core::types::snapshot::SnapshotMetadata, engram_core::SandboxError>
            {
                self.inner.snapshot(id).await
            }
            async fn commit_snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.commits.fetch_add(1, Ordering::SeqCst);
                self.inner.commit_snapshot(id).await
            }
            async fn abort_snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.aborts.fetch_add(1, Ordering::SeqCst);
                self.inner.abort_snapshot(id).await
            }
            async fn restore(
                &self,
                metadata: engram_core::types::snapshot::SnapshotMetadata,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.restore(metadata).await
            }
            async fn start_agent(
                &self,
                id: engram_core::SandboxId,
                agent: engram_core::types::sandbox::AgentSpec,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.start_agent(id, agent, policy).await
            }
            async fn apply_egress_policy(
                &self,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.apply_egress_policy(policy).await
            }
            async fn guest_ip(&self, id: engram_core::SandboxId) -> Option<String> {
                self.inner.guest_ip(id).await
            }
            async fn bind_session(
                &self,
                session_id: engram_core::SessionId,
                sandbox_id: engram_core::SandboxId,
            ) {
                self.inner.bind_session(session_id, sandbox_id).await
            }
            async fn unbind_session(&self, session_id: engram_core::SessionId) {
                self.inner.unbind_session(session_id).await
            }
            async fn send_prompt(
                &self,
                sandbox_id: engram_core::SandboxId,
                text: String,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.send_prompt(sandbox_id, text).await
            }
            async fn acquire_shell(
                &self,
                sandbox_id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.acquire_shell(sandbox_id).await
            }
            async fn release_shell(
                &self,
                sandbox_id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.release_shell(sandbox_id).await
            }
        }

        // Wire up state with the spy wrapping the standard
        // HostRegistry → LocalHostClient → ProcessBackend stack, then
        // toggle MiniMeta to fail the next record_snapshot.
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-test".into(),
            harness: HarnessSpec::None,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
        };

        let sandbox_root = TempDir::new().unwrap();
        let local_path = sandbox_root.path().join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.path().join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        *meta.fail_next_record_snapshot.lock() = true;
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );

        let aborts = StdArc::new(AtomicU32::new(0));
        let commits = StdArc::new(AtomicU32::new(0));
        let spy: Arc<dyn engram_core::traits::HostClient> = Arc::new(SpyHost {
            inner: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            aborts: aborts.clone(),
            commits: commits.clone(),
        });

        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: spy,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-evict-abort-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-evict-abort-test"),
                ),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
        };
        let cfg = crate::config::CoordinatorConfig {
            local_path,
            ..crate::config::CoordinatorConfig::default()
        };
        let state = Arc::new(crate::state::AppState::new_with_registry(
            cfg,
            services,
            host_registry,
        ));

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state.registry.bind(session_id, sandbox_id);

        let err = evict_idle_session(&state, session_id, sandbox_id)
            .await
            .expect_err("record_snapshot failure must bubble out of evict_idle_session");
        match err {
            EvictError::Meta(_) => {}
            other => panic!("expected EvictError::Meta, got {other:?}"),
        }

        assert_eq!(
            aborts.load(Ordering::SeqCst),
            1,
            "host.abort_snapshot must fire exactly once after record_snapshot failure",
        );
        assert_eq!(
            commits.load(Ordering::SeqCst),
            0,
            "host.commit_snapshot must NOT fire when pipeline failed",
        );
    }

    /// Sanity inverse: full pipeline success → commit fires, no abort.
    #[tokio::test]
    async fn evict_idle_session_commits_snapshot_on_full_success() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc as StdArc;

        // Tiny duplicate of the spy from the abort test — keeps the
        // tests independently readable.
        struct SpyHost {
            inner: StdArc<dyn engram_core::traits::HostClient>,
            aborts: StdArc<AtomicU32>,
            commits: StdArc<AtomicU32>,
        }
        #[async_trait::async_trait]
        impl engram_core::traits::HostClient for SpyHost {
            async fn create(
                &self,
                spec: SandboxSpec,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.create(spec).await
            }
            async fn destroy(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.destroy(id).await
            }
            async fn list(&self) -> Result<Vec<engram_core::SandboxId>, engram_core::SandboxError> {
                self.inner.list().await
            }
            async fn exec_stream(
                &self,
                id: engram_core::SandboxId,
                cmd: engram_core::types::sandbox::ExecRequest,
            ) -> Result<engram_core::types::sandbox::ExecStream, engram_core::SandboxError>
            {
                self.inner.exec_stream(id, cmd).await
            }
            async fn snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<engram_core::types::snapshot::SnapshotMetadata, engram_core::SandboxError>
            {
                self.inner.snapshot(id).await
            }
            async fn commit_snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.commits.fetch_add(1, Ordering::SeqCst);
                self.inner.commit_snapshot(id).await
            }
            async fn abort_snapshot(
                &self,
                id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.aborts.fetch_add(1, Ordering::SeqCst);
                self.inner.abort_snapshot(id).await
            }
            async fn restore(
                &self,
                metadata: engram_core::types::snapshot::SnapshotMetadata,
            ) -> Result<engram_core::SandboxId, engram_core::SandboxError> {
                self.inner.restore(metadata).await
            }
            async fn start_agent(
                &self,
                id: engram_core::SandboxId,
                agent: engram_core::types::sandbox::AgentSpec,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.start_agent(id, agent, policy).await
            }
            async fn apply_egress_policy(
                &self,
                policy: engram_core::types::egress::SessionEgressPolicy,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.apply_egress_policy(policy).await
            }
            async fn guest_ip(&self, id: engram_core::SandboxId) -> Option<String> {
                self.inner.guest_ip(id).await
            }
            async fn bind_session(
                &self,
                session_id: engram_core::SessionId,
                sandbox_id: engram_core::SandboxId,
            ) {
                self.inner.bind_session(session_id, sandbox_id).await
            }
            async fn unbind_session(&self, session_id: engram_core::SessionId) {
                self.inner.unbind_session(session_id).await
            }
            async fn send_prompt(
                &self,
                sandbox_id: engram_core::SandboxId,
                text: String,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.send_prompt(sandbox_id, text).await
            }
            async fn acquire_shell(
                &self,
                sandbox_id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.acquire_shell(sandbox_id).await
            }
            async fn release_shell(
                &self,
                sandbox_id: engram_core::SandboxId,
            ) -> Result<(), engram_core::SandboxError> {
                self.inner.release_shell(sandbox_id).await
            }
        }

        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-test".into(),
            harness: HarnessSpec::None,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
        };

        let sandbox_root = TempDir::new().unwrap();
        let local_path = sandbox_root.path().join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.path().join("sandboxes")));
        let meta = Arc::new(MiniMeta::new(session));
        let host_registry = Arc::new(HostRegistry::new(
            meta.clone() as Arc<dyn engram_core::traits::MetadataStore>
        ));
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );

        let aborts = StdArc::new(AtomicU32::new(0));
        let commits = StdArc::new(AtomicU32::new(0));
        let spy: Arc<dyn engram_core::traits::HostClient> = Arc::new(SpyHost {
            inner: host_registry.clone() as Arc<dyn engram_core::traits::HostClient>,
            aborts: aborts.clone(),
            commits: commits.clone(),
        });

        let services = Services {
            meta: meta.clone(),
            cloud: Arc::new(MockCloud::new()),
            host: spy,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: Arc::new(engram_oci::OciClient::new(Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            auth_resolver: Arc::new(engram_oci::AnonymousResolver),
            blob: Arc::new(engram_storage_local::LocalBlobStorage::new(
                std::env::temp_dir().join("engram-evict-commit-test"),
            )),
            chunk_store: engram_chunk_store::ChunkStore::new(Arc::new(
                engram_storage_local::LocalBlobStorage::new(
                    std::env::temp_dir().join("engram-evict-commit-test"),
                ),
            )),
            host_pool: Arc::new(engram_protocol::grpc_pool::GrpcHostPool::new()),
            materialize_dir: None,
        };
        let cfg = crate::config::CoordinatorConfig {
            local_path,
            ..crate::config::CoordinatorConfig::default()
        };
        let state = Arc::new(crate::state::AppState::new_with_registry(
            cfg,
            services,
            host_registry,
        ));

        let sandbox_id = state.services.host.create(process_spec()).await.unwrap();
        state.registry.bind(session_id, sandbox_id);
        state
            .services
            .meta
            .assign_session_sandbox(session_id, Some(sandbox_id))
            .await
            .unwrap();

        evict_idle_session(&state, session_id, sandbox_id)
            .await
            .expect("full pipeline must succeed");

        assert_eq!(
            commits.load(Ordering::SeqCst),
            1,
            "commit_snapshot must fire exactly once on full pipeline success",
        );
        assert_eq!(
            aborts.load(Ordering::SeqCst),
            0,
            "abort_snapshot must NOT fire on full pipeline success",
        );
    }

    #[tokio::test]
    async fn evict_idle_session_is_a_noop_when_sandbox_already_unbound() {
        // Race-safe path: another evictor / operator already
        // unbound the sandbox. evict_idle_session should return
        // Ok(()) without touching anything else.
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: SessionState::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-test".into(),
            harness: HarnessSpec::None,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
        };
        let sandbox_root = TempDir::new().unwrap();
        let state = build_state_with_session(session, sandbox_root.path());

        // Don't bind any sandbox; pass a random SandboxId.
        evict_idle_session(&state, session_id, engram_core::SandboxId::new())
            .await
            .expect("noop on already-unbound");

        // Session stays Active (no eviction happened).
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionState::Active);
    }
}
