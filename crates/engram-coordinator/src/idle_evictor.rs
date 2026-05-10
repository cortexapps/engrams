//! Idle-session evictor — Track B's pack-hosts mechanism.
//!
//! Background task on the coordinator that polls the
//! [`engram_host_agent::harness::HarnessHub`] for sandboxes whose
//! last harness event is older than `idle_ttl_secs`, and runs the
//! suspend pipeline on each:
//!
//!   1. **Take a Firecracker memory snapshot** to local NVMe so the
//!      session can be hot-restored on the same host without paying
//!      cold-clone-and-deps cost.
//!   2. **Destroy the sandbox** to free host RAM.
//!   3. **Mark the session `Idle`**, clear its `sandbox_id`, emit
//!      `SnapshotTaken` + `Evicted` + `StatusChanged` events so SSE
//!      subscribers see the suspension cleanly.
//!
//! ADR 0005 retired the workspace-checkpoint pre-step the original
//! Track C.9 design ran here — every snapshot now ships the rootfs
//! delta, and (Stage 5+) cold-tier blob durability covers the
//! cross-host case the git push used to.
//!
//! Auto-resume on next request is wired separately (`api/sessions.rs`
//! exec/exec_stream/SSE handlers): if status is `Idle`, call the
//! existing `resume` path before routing.
//!
//! `--mode=all` and dev-loop only for now: the task runs on the
//! coordinator with direct in-process access to the HarnessHub. In
//! multi-host production, the evictor's driver moves to each
//! host-agent (with the same shared `evict_idle_session` pipeline
//! function); this module's split into "the driver" and "the
//! pipeline" is intentional to make that future move surgical.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_core::traits::SandboxBackend;
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::SessionStatus;
use engram_core::{SandboxId, SessionId};
use tokio::task::JoinHandle;

use crate::state::{SessionEvent, SharedState};

/// **Soft** idle TTL — a session whose adapter emitted `Idle` and
/// stayed quiet for this long is hot-suspended. Default 30s tracks
/// "user is afk." Override via `ENGRAM_IDLE_TTL_SECS`.
pub const DEFAULT_IDLE_TTL_SECS: u64 = 30;

/// **Hard** idle TTL — backstop for adapters that go silent
/// without ever emitting `Idle` (stuck in a tool call, infinite
/// loop, etc.). Default 30 minutes; override via
/// `ENGRAM_IDLE_HARD_TTL_SECS`.
pub const DEFAULT_IDLE_HARD_TTL_SECS: u64 = 1800;

/// How often the evictor scans for over-TTL sandboxes. 10s is loose
/// enough that the scan itself is negligible load and tight enough
/// that the actual suspend lag past TTL is bounded.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Spawn the idle evictor as a background task. Returns the
/// JoinHandle so the caller can abort on shutdown (the run loop
/// itself never exits voluntarily).
pub fn spawn(
    state: SharedState,
    soft_ttl: Duration,
    hard_ttl: Duration,
    poll_interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(poll_interval);
        // Skip the immediate first tick so a freshly-started coord
        // doesn't insta-evict sandboxes whose harness just attached
        // a few ms before we polled.
        tick.tick().await;
        loop {
            tick.tick().await;
            run_once(&state, soft_ttl, hard_ttl).await;
        }
    })
}

async fn run_once(state: &SharedState, soft_ttl: Duration, hard_ttl: Duration) {
    let candidates = state.harness_hub.idle_sandboxes(soft_ttl, hard_ttl);
    for (session_id, sandbox_id) in candidates {
        if let Err(e) = evict_idle_session(state, session_id, sandbox_id).await {
            // Per-eviction failure logs but doesn't kill the loop;
            // a transient sandbox issue shouldn't pause the whole
            // host's pack-hosts machinery.
            tracing::warn!(
                session_id = %session_id,
                sandbox_id = %sandbox_id,
                error = %e,
                "idle eviction failed; will retry next tick if still idle",
            );
        }
    }
}

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

    // Step 1: take a Firecracker memory snapshot to local NVMe.
    // Path layout matches the operator-driven /sessions/:id/snapshot
    // handler so the resume path can find it via the same
    // `local_path` field on SnapshotRecord.
    let dest = state
        .snapshot_dir()
        .join(session_id.to_string())
        .join(uuid::Uuid::new_v4().to_string());
    tokio::fs::create_dir_all(&dest)
        .await
        .map_err(|e| EvictError::Io(format!("create snapshot dir: {e}")))?;
    let metadata = state
        .services
        .sandbox
        .snapshot(sandbox_id, &dest)
        .await
        .map_err(EvictError::Sandbox)?;

    let host_id = state.host_registry.host_of(sandbox_id);
    let now = Utc::now();
    let record = SnapshotRecord {
        id: metadata.id,
        session_id,
        host_id,
        local_path: Some(dest),
        image_version: metadata.image_version.clone(),
        size_bytes: metadata.size_bytes,
        created_at: metadata.created_at,
        last_accessed_at: now,
        // Hot-tier-only at create time; the cold-tier flush primitive
        // (Stage 5) flips these via `MetadataStore::flush_to_cold`.
        blob_present: false,
        replicated_at: None,
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
    if let Err(e) = state.services.sandbox.destroy(sandbox_id).await {
        tracing::warn!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            error = %e,
            "idle eviction: destroy failed; continuing to mark session Idle",
        );
    }
    if let Some(proxy) = state.services.egress_proxy.as_ref() {
        proxy.registry.unregister(session_id);
    }

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

/// Helper: pull the soft TTL from the env, falling back to the
/// default. Called at coord startup.
pub fn idle_ttl_from_env() -> Duration {
    std::env::var("ENGRAM_IDLE_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_IDLE_TTL_SECS))
}

/// Helper: pull the hard TTL from the env, falling back to the
/// default. The hard TTL is the stuck-adapter backstop — far
/// looser than the soft TTL so legit long tool calls don't trip it.
pub fn idle_hard_ttl_from_env() -> Duration {
    std::env::var("ENGRAM_IDLE_HARD_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_IDLE_HARD_TTL_SECS))
}

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
    use engram_core::types::session::{HarnessSpec, SessionKind, WorkspaceSpec};
    use engram_core::types::{Session, SessionStatus};
    use engram_sandbox_process::ProcessBackend;
    use engram_secrets_dev::InMemorySecretStore;
    use std::path::Path;
    use tempfile::TempDir;

    fn build_state_with_session(session: Session, sandbox_root: &Path) -> SharedState {
        let local_path = sandbox_root.join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.join("sandboxes")));
        let host_registry = Arc::new(HostRegistry::new());
        host_registry.register(engram_core::HostId::new(), backend.clone());
        let services = Services {
            meta: Arc::new(MiniMeta::new(session)),
            cloud: Arc::new(MockCloud::new()),
            sandbox: host_registry.clone() as Arc<dyn SandboxBackend>,
            secrets: Arc::new(InMemorySecretStore::new()),
            kek: Arc::new(engram_crypto::EnvVarKeyProvider::from_bytes(
                [0u8; 32], "test:v1",
            )),
            oci: std::sync::Arc::new(engram_oci::OciClient::new(std::sync::Arc::new(
                engram_oci::AnonymousResolver,
            ))),
            egress_proxy: None,
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
            status: SessionStatus::Active,
            host_id: None,
            sandbox_id: None,
            image: "test/repo:evict-test".into(),
            workspace: WorkspaceSpec::Empty,
            harness: HarnessSpec::None,
            session_kind: SessionKind::Ephemeral,
            checkpoint_branch: None,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
        };

        let sandbox_root = TempDir::new().unwrap();
        let state = build_state_with_session(session, sandbox_root.path());

        let sandbox_id = state.services.sandbox.create(process_spec()).await.unwrap();
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
        state.services.sandbox.exec(sandbox_id, req).await.unwrap();

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
        assert!(snaps[0].local_path.is_some());

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
            workspace: WorkspaceSpec::Empty,
            harness: HarnessSpec::None,
            session_kind: SessionKind::Ephemeral,
            checkpoint_branch: None,
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
