//! Idle-session evictor — Track B's pack-hosts mechanism.
//!
//! Background task on the coordinator that polls the
//! [`engram_host_agent::harness::HarnessHub`] for sandboxes whose
//! last harness event is older than `idle_ttl_secs`, and runs the
//! suspend pipeline on each:
//!
//!   1. **Checkpoint to git** (Track C.9 path) — pushes any tail-end
//!      workspace changes to `engram/sessions/<id>`. Harmless if the
//!      auto-checkpoint already fired on `Idle`; the empty-diff guard
//!      makes this a one-`git status` no-op when nothing changed.
//!   2. **Take a Firecracker memory snapshot** to local NVMe so the
//!      session can be hot-restored on the same host without paying
//!      cold-clone-and-deps cost.
//!   3. **Destroy the sandbox** to free host RAM.
//!   4. **Mark the session `Idle`**, clear its `sandbox_id`, emit
//!      `SnapshotTaken` + `Evicted` + `StatusChanged` events so SSE
//!      subscribers see the suspension cleanly.
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

use crate::state::{auto_checkpoint, SessionEvent, SharedState};

/// Default idle TTL — a session that's emitted no harness events
/// for this long is hot-suspended. 60s is loose enough to avoid
/// thrashing on conversational pauses but tight enough to actually
/// pack hosts. Override per-deployment via `ENGRAM_IDLE_TTL_SECS`.
pub const DEFAULT_IDLE_TTL_SECS: u64 = 60;

/// How often the evictor scans for over-TTL sandboxes. 10s is loose
/// enough that the scan itself is negligible load and tight enough
/// that the actual suspend lag past TTL is bounded.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Spawn the idle evictor as a background task. Returns the
/// JoinHandle so the caller can abort on shutdown (the run loop
/// itself never exits voluntarily).
pub fn spawn(state: SharedState, idle_ttl: Duration, poll_interval: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(poll_interval);
        // Skip the immediate first tick so a freshly-started coord
        // doesn't insta-evict sandboxes whose harness just attached
        // a few ms before we polled.
        tick.tick().await;
        loop {
            tick.tick().await;
            run_once(&state, idle_ttl).await;
        }
    })
}

async fn run_once(state: &SharedState, idle_ttl: Duration) {
    let candidates = state.harness_hub.idle_sandboxes(idle_ttl);
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

    // Step 1: checkpoint workspace. The auto-checkpoint on `Idle`
    // already pushed at run-completion; this is a safety net for
    // any tail-end changes. Empty diffs short-circuit before the
    // commit + push, so the cost is one `git status` round-trip.
    auto_checkpoint(
        session_id,
        sandbox_id,
        &state.services.meta,
        &state.services.sandbox,
        &state.events,
    )
    .await;

    // Step 2: take a Firecracker memory snapshot to local NVMe.
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

/// Helper: pull `idle_ttl_secs` from the env, falling back to the
/// default. Called at coord startup to compose the spawn args.
pub fn idle_ttl_from_env() -> Duration {
    std::env::var("ENGRAM_IDLE_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_IDLE_TTL_SECS))
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
    use crate::image_registry::ImageRegistry;
    use crate::state::tests::MiniMeta;
    use crate::state::AppState;
    use crate::Services;
    use engram_cloud_mock::MockCloud;
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
    use engram_core::types::session::{checkpoint_branch_for, RepoUrl, SessionKind};
    use engram_core::types::{Session, SessionStatus};
    use engram_sandbox_process::ProcessBackend;
    use engram_secrets_dev::InMemorySecretStore;
    use std::path::Path;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    fn run_host(args: &[&str], cwd: &Path) {
        let out = StdCommand::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "host {} in {} failed: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn seed_remote_and_workspace() -> (TempDir, TempDir) {
        let remote = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        run_host(
            &["git", "init", "--bare", "--initial-branch=main"],
            remote.path(),
        );
        run_host(&["git", "init", "--initial-branch=main"], workspace.path());
        run_host(
            &[
                "git",
                "remote",
                "add",
                "origin",
                &format!("{}", remote.path().display()),
            ],
            workspace.path(),
        );
        run_host(
            &[
                "git",
                "-c",
                "user.email=t@e.local",
                "-c",
                "user.name=t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
            workspace.path(),
        );
        run_host(&["git", "push", "-u", "origin", "main"], workspace.path());
        (remote, workspace)
    }

    fn build_state_with_session(session: Session, sandbox_root: &Path) -> SharedState {
        let local_path = sandbox_root.join("local");
        std::fs::create_dir_all(&local_path).unwrap();
        let images_dir = sandbox_root.join("images");
        std::fs::create_dir_all(&images_dir).unwrap();
        let backend: Arc<dyn SandboxBackend> =
            Arc::new(ProcessBackend::new(sandbox_root.join("sandboxes")));
        let host_registry = Arc::new(HostRegistry::new());
        host_registry.register(engram_core::HostId::new(), backend.clone());
        let services = Services {
            meta: Arc::new(MiniMeta::new(session)),
            cloud: Arc::new(MockCloud::new()),
            sandbox: host_registry.clone() as Arc<dyn SandboxBackend>,
            secrets: Arc::new(InMemorySecretStore::new()),
            images: ImageRegistry::new(images_dir),
        };
        let cfg = CoordinatorConfig {
            local_path,
            ..CoordinatorConfig::default()
        };
        Arc::new(AppState::new_with_registry(cfg, services, host_registry))
    }

    fn process_spec(rootfs: &Path) -> SandboxSpec {
        SandboxSpec {
            image: "evict-test".into(),
            rootfs_source: Some(rootfs.to_path_buf()),
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 256 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
        }
    }

    #[tokio::test]
    async fn evict_idle_session_runs_full_pipeline() {
        let (remote, workspace) = seed_remote_and_workspace();

        let session_id = engram_core::SessionId::new();
        let branch = checkpoint_branch_for(session_id);
        let session = Session {
            id: session_id,
            repo: format!("git+file://{}", remote.path().display()),
            branch: "main".into(),
            user_id: None,
            status: SessionStatus::Active,
            image_version: "evict-test".into(),
            host_id: None,
            sandbox_id: None,
            session_kind: SessionKind::Git,
            repo_url: Some(RepoUrl::Git {
                url: format!("file://{}", remote.path().display()),
            }),
            checkpoint_branch: Some(branch.clone()),
            last_harness_event_at: None,
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
        };

        let sandbox_root = TempDir::new().unwrap();
        let state = build_state_with_session(session, sandbox_root.path());

        // Create the sandbox via the registry, bind it to the
        // session, simulate an agent edit. The eviction pipeline
        // will checkpoint that edit, snapshot the sandbox, destroy
        // it, and mark the session Idle.
        let sandbox_id = state
            .services
            .sandbox
            .create(process_spec(workspace.path()))
            .await
            .unwrap();
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
        assert_eq!(snaps.len(), 1, "exactly one FC snapshot recorded");
        assert!(snaps[0].local_path.is_some());

        // The remote got a checkpoint commit on `engram/sessions/<id>`.
        let out = StdCommand::new("git")
            .args(["-C"])
            .arg(remote.path())
            .args(["rev-list", "--count", &branch])
            .output()
            .unwrap();
        let count: u32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0);
        assert!(
            count >= 1,
            "checkpoint branch must have at least one commit"
        );

        // The bus saw the right sequence of events. Drain the
        // subscriber and confirm the kinds.
        let mut kinds: Vec<String> = Vec::new();
        while let Ok(ev) = tokio::time::timeout(Duration::from_millis(50), sub.recv()).await {
            if let Ok(indexed) = ev {
                kinds.push(indexed.event.kind().to_string());
            }
        }
        // CheckpointPushed comes from the auto_checkpoint inside
        // the eviction pipeline; SnapshotTaken/Evicted/StatusChanged
        // are emitted by evict_idle_session itself.
        for required in [
            "checkpoint_pushed",
            "snapshot_taken",
            "evicted",
            "status_changed",
        ] {
            assert!(
                kinds.iter().any(|k| k == required),
                "missing event {required} in {kinds:?}"
            );
        }
    }

    #[tokio::test]
    async fn evict_idle_session_is_a_noop_when_sandbox_already_unbound() {
        // Race-safe path: another evictor / operator already
        // unbound the sandbox. evict_idle_session should return
        // Ok(()) without touching anything else.
        let session_id = engram_core::SessionId::new();
        let session = Session {
            id: session_id,
            repo: "local://hello".into(),
            branch: "main".into(),
            user_id: None,
            status: SessionStatus::Active,
            image_version: "evict-test".into(),
            host_id: None,
            sandbox_id: None,
            session_kind: SessionKind::Local,
            repo_url: Some(RepoUrl::Local {
                name: "hello".into(),
            }),
            checkpoint_branch: None,
            last_harness_event_at: None,
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
