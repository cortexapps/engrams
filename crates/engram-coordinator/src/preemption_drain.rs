//! Preemption best-effort drain — Track D.
//!
//! When the cloud surfaces a preemption notice (`engram-cloud-gcp`
//! polling the GCE metadata server, MockCloud's
//! `trigger_preemption` for tests), this task fans out across all
//! live sessions on this coordinator and runs the drain pipeline in
//! parallel within the deadline:
//!
//!   1. Best-effort `auto_checkpoint` — push the workspace to
//!      `engram/sessions/<id>` so the agent's last unit of work is
//!      durable past the VM's death. The empty-diff guard means
//!      already-checkpointed sessions are a `git status` round-trip.
//!   2. Best-effort `SandboxBackend::destroy` — release Firecracker
//!      handles cleanly. The VM is about to die anyway; this is just
//!      hygiene.
//!   3. Mark the session `Dead`, clear `host_id` +
//!      `sandbox_id`. The caller decides what to do next via
//!      `POST /sessions/:id/resume` (cross-host cold resume) or
//!      `engram session fork` — Engram does *not* auto-resume on
//!      preemption (that's the documented contract; sessions are
//!      ephemeral, host loss = caller-driven recovery).
//!
//! Important: this does NOT take an FC memory snapshot. The host's
//! about to die — a snapshot dies with it. Cross-host durability is
//! git, not blob-replicated VM memory (see DESIGN.md Phase 4).
//!
//! `--mode=all` deployment for now: the consumer runs on the
//! coordinator and drains every active session in metadata. Multi-
//! host production will move the consumer to each host-agent (with
//! its local SandboxBackend.list() determining "which sessions are
//! on this host"), keeping the `drain_session` pipeline as the
//! shared canonical primitive.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engram_core::types::host::PreemptionNotice;
use engram_core::types::SessionStatus;
use engram_core::{SandboxId, SessionId};
use futures::StreamExt;
use tokio::task::JoinHandle;

use crate::state::{auto_checkpoint, SessionEvent, SharedState};

/// How much of the preemption window to spend on the drain itself,
/// leaving headroom for the VM's actual shutdown. GCP's default
/// Spot window is 30s; we cap drain work at 25s so the kernel /
/// systemd /firecracker get the rest.
pub const DEFAULT_DRAIN_DEADLINE: Duration = Duration::from_secs(25);

/// Spawn the preemption drainer. Subscribes to
/// `cloud.preemption_signal()` and fans out a drain on each notice.
/// Drops the JoinHandle to its caller; the task lives for coord's
/// lifetime.
pub fn spawn(state: SharedState) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut signal = state.services.cloud.preemption_signal();
        while let Some(notice) = signal.next().await {
            run_drain(&state, notice).await;
            // We continue the loop in case the cloud emits more
            // than one signal — `engram-cloud-gcp`'s polling
            // semantics let multiple ticks fire if the underlying
            // metadata stays "TERMINATE" for a while. Drain is
            // idempotent on already-Dead sessions.
        }
    })
}

async fn run_drain(state: &SharedState, notice: PreemptionNotice) {
    tracing::warn!(
        reason = %notice.reason,
        deadline_secs = ?notice.deadline_secs,
        "preemption notice received — draining all live sessions",
    );
    let deadline = notice
        .deadline_secs
        .map(|s| Duration::from_secs(s as u64))
        .map(|d| d.saturating_sub(Duration::from_secs(5)))
        .unwrap_or(DEFAULT_DRAIN_DEADLINE);

    // Snapshot the active session set up front. New sessions
    // created after we sample shouldn't be drained — they raced the
    // notice and either picked a different host (multi-host) or
    // accepted the death (--mode=all). Either way, not our
    // problem.
    let sessions = match state.services.meta.list_active_sessions().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "preemption drain: list_active_sessions failed; nothing to drain");
            return;
        }
    };
    let pairs: Vec<(SessionId, SandboxId)> = sessions
        .into_iter()
        .filter(|s| s.status == SessionStatus::Active)
        .filter_map(|s| {
            state
                .registry
                .get(s.id)
                .map(|sandbox_id| (s.id, sandbox_id))
        })
        .collect();
    let count = pairs.len();
    if count == 0 {
        tracing::info!("preemption drain: no active sessions to drain");
        return;
    }
    tracing::info!(count, ?deadline, "draining sessions in parallel");

    let drains = pairs.into_iter().map(|(session_id, sandbox_id)| {
        let state = state.clone();
        async move {
            if let Err(e) = drain_session(&state, session_id, sandbox_id).await {
                tracing::warn!(
                    session_id = %session_id,
                    sandbox_id = %sandbox_id,
                    error = %e,
                    "preemption drain: session-level drain failed",
                );
            }
        }
    });

    if tokio::time::timeout(deadline, futures::future::join_all(drains))
        .await
        .is_err()
    {
        tracing::warn!(
            ?deadline,
            "preemption drain: deadline elapsed; some sessions may not be Dead yet"
        );
    } else {
        tracing::info!(count, "preemption drain complete");
    }
}

/// Drain one session: checkpoint workspace, destroy sandbox, mark
/// Dead. Pure function over `SharedState`; the caller
/// (the cloud-signal consumer or, in multi-host production, the
/// host-agent's preemption handler) wraps it in the appropriate
/// fan-out.
pub async fn drain_session(
    state: &SharedState,
    session_id: SessionId,
    sandbox_id: SandboxId,
) -> Result<(), DrainError> {
    // Race-safe: if the registry doesn't think this sandbox is bound
    // anymore (operator drained it, idle evictor evicted it, etc.),
    // there's nothing to drain.
    if state.registry.get(session_id) != Some(sandbox_id) {
        tracing::debug!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            "preemption drain: sandbox no longer bound; skipping",
        );
        return Ok(());
    }

    // Step 1: best-effort workspace checkpoint to git. The
    // empty-diff guard makes this nearly free for sessions whose
    // last `Idle` already pushed.
    auto_checkpoint(
        session_id,
        sandbox_id,
        &state.services.meta,
        &state.services.sandbox,
        &state.events,
    )
    .await;

    // Step 2: best-effort destroy. The VM is about to die — this
    // just lets us release Firecracker handles cleanly. Failure is
    // expected sometimes (mid-shutdown FC API may be unresponsive).
    state.registry.unbind(session_id);
    if let Err(e) = state.services.sandbox.destroy(sandbox_id).await {
        tracing::debug!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            error = %e,
            "preemption drain: destroy failed (expected during shutdown)",
        );
    }

    // Step 3: mark Dead. The caller's resume call later
    // brings the session back on a different host via the git
    // checkpoint branch.
    if let Err(e) = state
        .services
        .meta
        .assign_session_host(session_id, None)
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "preemption drain: assign_session_host(None) failed",
        );
    }
    if let Err(e) = state
        .services
        .meta
        .assign_session_sandbox(session_id, None)
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "preemption drain: assign_session_sandbox(None) failed",
        );
    }
    state
        .services
        .meta
        .set_session_status(session_id, SessionStatus::Dead)
        .await
        .map_err(|e| DrainError::Meta(e.to_string()))?;

    state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: SessionStatus::Active,
                to: SessionStatus::Dead,
                at: Utc::now(),
            },
        )
        .await
        .map_err(|e| DrainError::Emit(e.to_string()))?;

    Ok(())
}

#[derive(Debug)]
pub enum DrainError {
    Meta(String),
    Emit(String),
}

impl std::fmt::Display for DrainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Meta(m) => write!(f, "preemption drain meta: {m}"),
            Self::Emit(m) => write!(f, "preemption drain emit: {m}"),
        }
    }
}

impl std::error::Error for DrainError {}

// Marker so `Arc` and other unused trait bounds don't trip dead-code
// checks during partial builds.
#[allow(dead_code)]
fn _arc_unused_check<T>(_: Arc<T>) {}

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
    use engram_core::traits::{CloudBackend, SandboxBackend};
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
    use engram_core::types::session::{
        checkpoint_branch_for, HarnessSpec, ImageRef, SessionKind, WorkspaceSpec,
    };
    use engram_core::types::Session;
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

    fn build_state_with_session_and_cloud(
        session: Session,
        sandbox_root: &Path,
        cloud: Arc<dyn CloudBackend>,
    ) -> SharedState {
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
            cloud,
            sandbox: host_registry.clone() as Arc<dyn SandboxBackend>,
            secrets: Arc::new(InMemorySecretStore::new()),
            images: ImageRegistry::new(images_dir),
            harnesses: Arc::new(crate::harness_registry::HarnessRegistry::empty()),
        };
        let cfg = CoordinatorConfig {
            local_path,
            ..CoordinatorConfig::default()
        };
        Arc::new(AppState::new_with_registry(cfg, services, host_registry))
    }

    fn process_spec(rootfs: &Path) -> SandboxSpec {
        SandboxSpec {
            image: "drain-test".into(),
            rootfs_source: Some(rootfs.to_path_buf()),
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 256 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            mounts: Vec::new(),
        }
    }

    #[tokio::test]
    async fn drain_session_pushes_workspace_and_marks_dead() {
        let (remote, workspace) = seed_remote_and_workspace();
        let session_id = engram_core::SessionId::new();
        let branch = checkpoint_branch_for(session_id);
        let session = Session {
            id: session_id,
            user_id: None,
            status: SessionStatus::Active,
            host_id: Some(engram_core::HostId::new()),
            sandbox_id: None,
            image: ImageRef::Registry {
                repo: "test/repo".into(),
                tag: "drain-test".into(),
            },
            workspace: WorkspaceSpec::Git {
                url: format!("file://{}", remote.path().display()),
                branch: "main".into(),
                read_only: false,
            },
            harness: HarnessSpec::None,
            session_kind: SessionKind::Git,
            checkpoint_branch: Some(branch.clone()),
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
        };

        let sandbox_root = TempDir::new().unwrap();
        let cloud: Arc<dyn CloudBackend> = Arc::new(MockCloud::new());
        let state = build_state_with_session_and_cloud(session, sandbox_root.path(), cloud);

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

        // Simulate agent edit: file change since the last checkpoint.
        let req = ExecRequest {
            command: vec![
                "sh".into(),
                "-c".into(),
                "echo preempted-work > preempt-marker.txt".into(),
            ],
            stdin: None,
            env: Default::default(),
            workdir: None,
            timeout: Some(Duration::from_secs(5)),
        };
        state.services.sandbox.exec(sandbox_id, req).await.unwrap();

        drain_session(&state, session_id, sandbox_id)
            .await
            .expect("drain should succeed");

        // Session is Dead with host/sandbox cleared.
        let after = state.services.meta.get_session(session_id).await.unwrap();
        assert_eq!(after.status, SessionStatus::Dead);
        assert_eq!(after.host_id, None);
        assert_eq!(after.sandbox_id, None);
        assert_eq!(state.registry.get(session_id), None);

        // The remote got a checkpoint commit on engram/sessions/<id>.
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
            "checkpoint branch must have at least one commit (got {count})"
        );

        // No SnapshotRecord recorded — preemption explicitly skips
        // FC snapshots since the host's about to die.
        let snaps = state
            .services
            .meta
            .list_snapshots_for_session(session_id)
            .await
            .unwrap();
        assert!(
            snaps.is_empty(),
            "preemption drain must not take FC snapshots"
        );
    }

    #[tokio::test]
    async fn preemption_signal_drives_drain_end_to_end() {
        // Wire MockCloud, spawn the drain task, trigger a
        // preemption notice, assert the active session lands in
        // Dead within the 25s deadline.
        let (remote, workspace) = seed_remote_and_workspace();
        let session_id = engram_core::SessionId::new();
        let branch = checkpoint_branch_for(session_id);
        let session = Session {
            id: session_id,
            user_id: None,
            status: SessionStatus::Active,
            host_id: Some(engram_core::HostId::new()),
            sandbox_id: None,
            image: ImageRef::Registry {
                repo: "test/repo".into(),
                tag: "drain-test".into(),
            },
            workspace: WorkspaceSpec::Git {
                url: format!("file://{}", remote.path().display()),
                branch: "main".into(),
                read_only: false,
            },
            harness: HarnessSpec::None,
            session_kind: SessionKind::Git,
            checkpoint_branch: Some(branch.clone()),
            created_at: chrono::Utc::now(),
            last_active_at: chrono::Utc::now(),
        };

        let sandbox_root = TempDir::new().unwrap();
        let mock_cloud = Arc::new(MockCloud::new());
        let cloud: Arc<dyn CloudBackend> = mock_cloud.clone();
        let state = build_state_with_session_and_cloud(session, sandbox_root.path(), cloud);

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

        let drain_task = spawn(state.clone());

        // Give the consumer a beat to subscribe to the broadcast.
        tokio::time::sleep(Duration::from_millis(50)).await;
        mock_cloud.trigger_preemption("simulated", Some(25));

        // Wait for the session to flip to Dead.
        let mut transitioned = false;
        for _ in 0..50 {
            let s = state.services.meta.get_session(session_id).await.unwrap();
            if s.status == SessionStatus::Dead {
                transitioned = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            transitioned,
            "drain task should transition session within ~2.5s of the signal"
        );

        // Cleanup.
        drain_task.abort();
    }
}
