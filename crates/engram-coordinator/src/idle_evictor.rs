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
use engram_core::types::SessionStatus;
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
    // Guard: if the registry doesn't think this sandbox is bound to
    // the session anymore, the session was already evicted by some
    // other path (operator, dead-host detector). No-op cleanly.
    if state.registry.get(session_id) != Some(sandbox_id) {
        tracing::debug!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            "idle eviction: sandbox no longer bound; skipping",
        );
        return Ok(());
    }

    // Step 1: take a snapshot. ADR 0007 Phase 6: backend owns its
    // staging dir; coord no longer pre-allocates one. Durability
    // flows through the chunked manifests on `SnapshotMetadata`.
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
    state
        .services
        .meta
        .record_snapshot(record.clone())
        .await
        .map_err(|e| EvictError::Meta(e.to_string()))?;

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
    state
        .services
        .meta
        .set_session_status(session_id, SessionStatus::Idle)
        .await
        .map_err(|e| EvictError::Meta(e.to_string()))?;

    state
        .emit(
            session_id,
            SessionEvent::SnapshotTaken {
                snapshot_id: metadata.id,
                size_bytes: metadata.size_bytes,
                at: now,
            },
        )
        .await
        .map_err(|e| EvictError::Emit(e.to_string()))?;
    state
        .emit(session_id, SessionEvent::Evicted { at: now })
        .await
        .map_err(|e| EvictError::Emit(e.to_string()))?;
    state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: SessionStatus::Active,
                to: SessionStatus::Idle,
                at: now,
            },
        )
        .await
        .map_err(|e| EvictError::Emit(e.to_string()))?;

    Ok(())
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
    use engram_core::types::{Session, SessionStatus};
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
        let host_registry = Arc::new(HostRegistry::new());
        host_registry.register(
            engram_core::HostId::new(),
            Arc::new(engram_host_agent::LocalHostClient::with_noop_hub(
                backend.clone(),
            )),
        );
        let services = Services {
            meta: Arc::new(MiniMeta::new(session)),
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
            canonical_memory_manifest: None,
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
            status: SessionStatus::Active,
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
        assert_eq!(after.status, SessionStatus::Idle);
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

    #[tokio::test]
    async fn evict_idle_session_is_a_noop_when_sandbox_already_unbound() {
        // Race-safe path: another evictor / operator already
        // unbound the sandbox. evict_idle_session should return
        // Ok(()) without touching anything else.
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            user_id: None,
            status: SessionStatus::Active,
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
        assert_eq!(after.status, SessionStatus::Active);
    }
}
