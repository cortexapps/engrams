//! Snapshot / evict / resume endpoints.
//!
//! State machine:
//!
//! ```text
//!   POST /sessions             -> Active   (sandbox live, registry bound)
//!   POST /sessions/:id/snapshot -> Active   (snapshot taken, sandbox stays live)
//!   DELETE /sessions/:id/local -> Idle     (sandbox destroyed, requires snapshot)
//!   POST /sessions/:id/resume   -> Active   (restored from snapshot, registry rebound)
//!   DELETE /sessions/:id        -> Completed (terminal)
//! ```
//!
//! `snapshot` is intentionally separate from `evict_local`: snapshot is a
//! save-point that keeps the live sandbox running, evict drops it. Most
//! callers that want both should snapshot then evict in two requests, or
//! evict then resume across the lifecycle of a session.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use engram_core::traits::SandboxBackend;
use engram_core::types::sandbox::{
    CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec as VmSpec,
};
use engram_core::types::session::{RepoUrl, SessionKind};
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::types::{Session, SessionStatus};
use engram_core::{SandboxId, SessionId};
use serde::Serialize;

use crate::error::ApiError;
use crate::host_registry::ScheduleContext;
use crate::image_registry::{ImageError, Rootfs};
use crate::state::{SessionEvent, SharedState};

/// Default sandbox sizing for resumed sessions. Mirrors
/// `api::sessions::create_session`'s defaults so a session resumed
/// after host loss gets the same shape it originally had unless the
/// image manifest overrides.
const DEFAULT_VCPUS: u32 = 2;
const DEFAULT_MEMORY_MIB: u32 = 4096;
const DEFAULT_DISK_GIB: u32 = 20;

#[derive(Serialize)]
pub struct SnapshotResponse {
    pub session_id: SessionId,
    pub snapshot_id: Option<String>,
    pub size_bytes: Option<u64>,
    pub note: &'static str,
}

pub async fn snapshot(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<Json<SnapshotResponse>, ApiError> {
    state.services.meta.get_session(id).await?;

    let sandbox_id = state.registry.get(id).ok_or_else(|| {
        ApiError::Conflict(
            "session has no live sandbox to snapshot — create or resume first".into(),
        )
    })?;

    // Snapshots live under `<local_path>/snapshots/<session>/<dir>`.
    // The directory name is its own UUID; the canonical SnapshotId is what
    // the backend reports back in SnapshotMetadata.
    let dest = state
        .snapshot_dir()
        .join(id.to_string())
        .join(uuid::Uuid::new_v4().to_string());
    tokio::fs::create_dir_all(&dest)
        .await
        .map_err(|e| ApiError::Internal(format!("create snapshot dir: {e}")))?;

    let metadata = state.services.sandbox.snapshot(sandbox_id, &dest).await?;

    let now = Utc::now();
    // Record the host that wrote this snapshot to its local disk so
    // the resume path's snapshot-affinity scheduler can route back to
    // it (zero-cost hot-tier hit).
    let host_id = state.host_registry.host_of(sandbox_id);
    let record = SnapshotRecord {
        id: metadata.id,
        session_id: id,
        host_id,
        local_path: Some(dest),
        image_version: metadata.image_version,
        size_bytes: metadata.size_bytes,
        created_at: metadata.created_at,
        last_accessed_at: now,
    };
    state.services.meta.record_snapshot(record).await?;

    state
        .emit(
            id,
            SessionEvent::SnapshotTaken {
                snapshot_id: metadata.id,
                size_bytes: metadata.size_bytes,
                at: now,
            },
        )
        .await?;

    // Snapshot does NOT change session state — the live sandbox keeps
    // running. evict_local is the explicit "drop from RAM" action.
    Ok(Json(SnapshotResponse {
        session_id: id,
        snapshot_id: Some(metadata.id.to_string()),
        size_bytes: Some(metadata.size_bytes),
        note: "snapshot recorded; live sandbox still running",
    }))
}

pub async fn resume(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<Json<SnapshotResponse>, ApiError> {
    resume_session(state, id).await.map(Json)
}

/// Auto-resume an `Idle` session if needed, before routing an
/// exec / exec_stream / events request to it. Track B's pack-hosts
/// counterpart: the idle evictor hot-suspends inactive sessions;
/// this helper brings them back transparently on the next request.
///
/// `Active` sessions are a no-op (Ok). `Idle` sessions are
/// resumed via the existing `/resume` flow and the function
/// returns once the session is Active again. Any other status
/// (Pending, Completed, Failed) returns an error — auto-resume
/// only undoes idle-eviction; it doesn't try to reanimate
/// terminal sessions.
pub async fn ensure_active(state: &SharedState, id: SessionId) -> Result<(), ApiError> {
    let session = state.services.meta.get_session(id).await?;
    if session.status == SessionStatus::Idle {
        resume_session(state.clone(), id).await?;
    }
    // For everything else (Active, Pending, PendingReassign,
    // Completed, Failed) we let the downstream handler decide. The
    // existing "session has no live sandbox" registry check
    // surfaces a clear error for sessions whose state precludes
    // routing without auto-resume kicking in. PendingReassign
    // specifically is *not* auto-resumed here — those sessions
    // are explicitly waiting for the caller to decide between
    // resume / fork / abandon, and silently re-routing them on a
    // different host would surprise the caller.
    Ok(())
}

async fn resume_session(state: SharedState, id: SessionId) -> Result<SnapshotResponse, ApiError> {
    let session = state.services.meta.get_session(id).await?;

    // Resuming an Active session is a no-op the caller probably
    // didn't mean. Idle (operator-evicted) and PendingReassign
    // (host died, awaiting reschedule) both want to re-spin a
    // sandbox — same code path, same scheduling.
    if !matches!(
        session.status,
        SessionStatus::Idle | SessionStatus::PendingReassign
    ) {
        return Err(ApiError::Conflict(format!(
            "session is {} — only Idle / PendingReassign sessions can be resumed",
            session.status.as_str()
        )));
    }

    // Snapshots are local-NVMe-only after Phase 4's blob removal.
    // Hot path (same host): the scheduler's snapshot-affinity
    // branch routes the restore back to the host that holds the
    // snapshot — sub-second hot resume from FC memory.bin.
    // Cold path (host gone): the snapshot's local_path is dead.
    // Fall through to the cross-host git-checkpoint path which
    // creates a fresh sandbox on the original image and resets
    // its workspace to the session's checkpoint branch.
    let snapshot = state.services.meta.latest_snapshot_for_session(id).await?;
    if let Some(record) = snapshot.as_ref().filter(|r| r.local_path.is_some()) {
        return resume_from_fc_snapshot(state, session, record.clone()).await;
    }

    // Fall through to git-checkpoint cross-host resume. Only valid
    // for Git-kind sessions; Local / Readonly have no checkpoint
    // branch to recover from.
    if session.session_kind != SessionKind::Git {
        return Err(ApiError::Conflict(format!(
            "session has no recoverable FC snapshot and isn't a Git session \
             ({}) — cannot resume cross-host",
            session.session_kind.as_str()
        )));
    }
    let branch = session.checkpoint_branch.clone().ok_or_else(|| {
        ApiError::Internal(
            "git session is missing its checkpoint_branch — schema invariant broken".into(),
        )
    })?;
    resume_from_git_checkpoint(state, session, &branch).await
}

/// Hot-resume path: a local FC snapshot exists, restore it on the
/// host that holds it (snapshot-affinity-scheduled). Sub-second.
async fn resume_from_fc_snapshot(
    state: SharedState,
    session: Session,
    record: SnapshotRecord,
) -> Result<SnapshotResponse, ApiError> {
    let id = session.id;
    let local_path = record.local_path.clone().ok_or_else(|| {
        ApiError::Internal("resume_from_fc_snapshot called without local_path".into())
    })?;
    let session_for_ctx = session.clone();
    let ctx = ScheduleContext {
        repo: &session_for_ctx.repo,
        image_version: &session_for_ctx.image_version,
        prefer_snapshot_id: Some(record.id),
        memory_mib: None,
    };
    let (host_id, new_sandbox_id) = state
        .host_registry
        .restore_for_session(&ctx, local_path)
        .await?;
    bind_resumed_session(&state, id, host_id, new_sandbox_id).await;
    finalize_resume(
        &state,
        id,
        session.status,
        SessionEvent::Resumed {
            snapshot_id: record.id,
            at: Utc::now(),
        },
    )
    .await?;

    Ok(SnapshotResponse {
        session_id: id,
        snapshot_id: Some(record.id.to_string()),
        size_bytes: Some(record.size_bytes),
        note: "resumed from snapshot",
    })
}

/// Cold-resume path: the original host is gone, so we boot a fresh
/// sandbox on the *same image* (warm-pool slot from the same
/// `image_version`) and use smart-bootstrap to bring its workspace
/// to the session's checkpoint branch. The image's bake-time clone
/// is at SHA X; the checkpoint branch contains X + the agent's
/// work; so `git fetch + git reset --hard FETCH_HEAD` is a small
/// diff. Sub-second is achievable when the warm pool has a slot.
async fn resume_from_git_checkpoint(
    state: SharedState,
    session: Session,
    branch: &str,
) -> Result<SnapshotResponse, ApiError> {
    let id = session.id;

    // Build a SandboxSpec from the session's *original* image
    // version — staying on the same image keeps the workspace's
    // baked clone at the same SHA the session was created from,
    // which bounds the diff size when we reset to the checkpoint
    // branch. If the image is no longer in the registry (rare;
    // would mean operator GC'd it), fall back to a sensible empty
    // spec — the smart-bootstrap will just clone fresh.
    let resolved = match state
        .services
        .images
        .load(&session.repo, &session.image_version)
        .await
    {
        Ok(r) => Some(r),
        Err(ImageError::NotFound { .. }) => None,
        Err(e) => return Err(ApiError::Internal(e.to_string())),
    };
    let manifest = resolved
        .as_ref()
        .map(|r| r.manifest.clone())
        .unwrap_or_default();
    let rootfs_source = resolved.as_ref().and_then(|r| match &r.rootfs {
        Rootfs::Directory(p) => Some(p.clone()),
        Rootfs::Ext4Image(p) => Some(p.clone()),
        Rootfs::None => None,
    });
    let vm_spec = VmSpec {
        image: session.image_version.clone(),
        rootfs_source,
        cpu: CpuLimit {
            vcpus: manifest.resources.suggested_vcpus.unwrap_or(DEFAULT_VCPUS),
        },
        memory: MemoryLimit {
            max_mib: manifest
                .resources
                .suggested_memory_mib
                .unwrap_or(DEFAULT_MEMORY_MIB),
        },
        disk: DiskLimit {
            max_gib: manifest
                .resources
                .suggested_disk_gib
                .unwrap_or(DEFAULT_DISK_GIB),
        },
        ttl: None,
        env: manifest.env.clone(),
        workdir: None,
    };

    let session_for_ctx = session.clone();
    let ctx = ScheduleContext {
        repo: &session_for_ctx.repo,
        image_version: &session_for_ctx.image_version,
        prefer_snapshot_id: None,
        memory_mib: Some(vm_spec.memory.max_mib),
    };
    let (host_id, sandbox_id) = state
        .host_registry
        .create_for_session(&ctx, vm_spec)
        .await?;

    // Workspace is now whatever the warm-pool image baked: a clone
    // of the writable repo at "bake SHA", or an empty cwd if the
    // image had no rootfs_source. Either way, smart-bootstrap brings
    // it to the checkpoint branch's HEAD.
    let url = match &session.repo_url {
        Some(RepoUrl::Git { url }) => url.clone(),
        _ => {
            return Err(ApiError::Internal(
                "git session is missing a parsed Git repo_url".into(),
            ));
        }
    };
    if let Err(e) =
        smart_bootstrap_to_branch(&state.services.sandbox, sandbox_id, &url, branch).await
    {
        // Failure leaves the sandbox running but in an unknown
        // state. Surface as Internal; the operator can destroy /
        // retry.
        return Err(ApiError::Internal(format!("smart-bootstrap: {e}")));
    }

    bind_resumed_session(&state, id, host_id, sandbox_id).await;
    let commit_sha = capture_head_sha(&state.services.sandbox, sandbox_id)
        .await
        .ok();
    finalize_resume(
        &state,
        id,
        session.status,
        SessionEvent::ResumedFromCheckpoint {
            branch: branch.to_string(),
            commit_sha: commit_sha.clone().unwrap_or_default(),
            at: Utc::now(),
        },
    )
    .await?;

    Ok(SnapshotResponse {
        session_id: id,
        snapshot_id: None,
        size_bytes: None,
        note: "resumed from git checkpoint",
    })
}

/// Smart-bootstrap: detect whether `/workspace` (the sandbox cwd
/// on ProcessBackend; `/workspace` inside a Firecracker guest)
/// already holds a clone of `url`, and either fast-forward via
/// `git fetch + git reset --hard <branch>` or do a clean
/// `git init + remote add + fetch + reset` if the directory is
/// empty / missing `.git`.
///
/// `git clone --branch X . ` requires an empty target dir, which
/// rules out "warm pool image baked the repo at a different
/// branch" — so we use `git init + remote add + fetch + reset`
/// instead, which works in any starting state. Slightly more
/// commands but composable.
pub(crate) async fn smart_bootstrap_to_branch(
    backend: &Arc<dyn SandboxBackend>,
    sandbox_id: SandboxId,
    url: &str,
    branch: &str,
) -> Result<(), String> {
    let detect = run_workspace(
        backend,
        sandbox_id,
        &[
            "sh",
            "-c",
            "test -d .git && git remote get-url origin 2>/dev/null",
        ],
    )
    .await
    .map_err(|e| format!("detect existing clone: {e}"))?;
    let already_cloned =
        detect.exit_status == Some(0) && String::from_utf8_lossy(&detect.stdout).trim() == url;

    if !already_cloned {
        // Clean slate: init in place (works even if cwd has files
        // left over from a previous boot), point origin at the
        // canonical URL, fetch the checkpoint branch, hard reset.
        run_workspace(backend, sandbox_id, &["git", "init", "."])
            .await
            .map_err(|e| format!("git init: {e}"))?;
        // `git remote add` errors if origin already exists; use
        // set-url which is idempotent.
        run_workspace(backend, sandbox_id, &["git", "remote", "remove", "origin"])
            .await
            .ok(); // ignore "no such remote"
        run_workspace(
            backend,
            sandbox_id,
            &["git", "remote", "add", "origin", url],
        )
        .await
        .map_err(|e| format!("git remote add origin: {e}"))?;
    }

    run_workspace(backend, sandbox_id, &["git", "fetch", "origin", branch])
        .await
        .map_err(|e| format!("git fetch origin {branch}: {e}"))?;
    run_workspace(
        backend,
        sandbox_id,
        &["git", "reset", "--hard", "FETCH_HEAD"],
    )
    .await
    .map_err(|e| format!("git reset --hard FETCH_HEAD: {e}"))?;
    Ok(())
}

/// Read `git rev-parse HEAD` from `sandbox_id`'s workspace and
/// trim. `Ok(None)` means the workspace has no HEAD yet (rare);
/// `Err` means the exec itself failed.
async fn capture_head_sha(
    backend: &Arc<dyn SandboxBackend>,
    sandbox_id: SandboxId,
) -> Result<String, ApiError> {
    let h = run_workspace(backend, sandbox_id, &["git", "rev-parse", "HEAD"])
        .await
        .map_err(|e| ApiError::Internal(format!("git rev-parse HEAD: {e}")))?;
    if h.exit_status != Some(0) {
        return Err(ApiError::Internal(format!(
            "git rev-parse HEAD: exit {:?}: {}",
            h.exit_status,
            String::from_utf8_lossy(&h.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&h.stdout).trim().to_string())
}

/// Run a workspace command (typically git) with the same hermetic
/// env the checkpoint primitive uses. Mirrors
/// `engram_host_agent::checkpoint::run_git`'s env — see that
/// function's doc for why each var is set.
async fn run_workspace(
    backend: &Arc<dyn SandboxBackend>,
    sandbox_id: SandboxId,
    argv: &[&str],
) -> Result<engram_core::types::sandbox::ExecHandle, engram_core::SandboxError> {
    let mut env = std::collections::HashMap::new();
    env.insert("GIT_CONFIG_GLOBAL".into(), "/dev/null".into());
    env.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
    env.insert("GIT_OPTIONAL_LOCKS".into(), "0".into());
    env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    let mut command: Vec<String> = Vec::with_capacity(argv.len() + 4);
    if argv.first().copied() == Some("git") {
        command.push("git".into());
        command.push("-c".into());
        command.push("core.fsmonitor=false".into());
        command.push("-c".into());
        command.push("gc.auto=0".into());
        for a in &argv[1..] {
            command.push(a.to_string());
        }
    } else {
        command.extend(argv.iter().map(|s| s.to_string()));
    }
    let req = ExecRequest {
        command,
        stdin: None,
        env,
        workdir: Some(".".into()),
        timeout: Some(Duration::from_secs(60)),
    };
    backend.exec(sandbox_id, req).await
}

async fn bind_resumed_session(
    state: &SharedState,
    id: SessionId,
    host_id: engram_core::HostId,
    sandbox_id: SandboxId,
) {
    if let Err(e) = state
        .services
        .meta
        .assign_session_host(id, Some(host_id))
        .await
    {
        tracing::warn!(
            session_id = %id,
            host_id = %host_id,
            error = %e,
            "assign_session_host on resume failed; HostRegistry routing still works",
        );
    }
    if let Err(e) = state
        .services
        .meta
        .assign_session_sandbox(id, Some(sandbox_id))
        .await
    {
        tracing::warn!(
            session_id = %id,
            sandbox_id = %sandbox_id,
            error = %e,
            "assign_session_sandbox on resume failed; live routing still works (in-memory only)",
        );
    }
    state.registry.bind(id, sandbox_id);
}

async fn finalize_resume(
    state: &SharedState,
    id: SessionId,
    from: SessionStatus,
    resume_event: SessionEvent,
) -> Result<(), ApiError> {
    state
        .services
        .meta
        .set_session_status(id, SessionStatus::Active)
        .await?;
    let now = Utc::now();
    state.emit(id, resume_event).await?;
    state
        .emit(
            id,
            SessionEvent::StatusChanged {
                from,
                to: SessionStatus::Active,
                at: now,
            },
        )
        .await?;
    Ok(())
}

pub async fn evict_local(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<StatusCode, ApiError> {
    let session = state.services.meta.get_session(id).await?;

    if session.status != SessionStatus::Active {
        return Err(ApiError::Conflict(format!(
            "session is {} — only Active sessions can be evicted",
            session.status.as_str()
        )));
    }

    // Refuse to evict if there's no snapshot to come back to. Without
    // this check the in-flight state would just be lost when the
    // sandbox gets destroyed.
    if state
        .services
        .meta
        .latest_snapshot_for_session(id)
        .await?
        .is_none()
    {
        return Err(ApiError::Conflict(
            "no snapshot exists for this session — take a snapshot before evicting".into(),
        ));
    }

    if let Some(sandbox_id) = state.registry.unbind(id) {
        if let Err(e) = state.services.sandbox.destroy(sandbox_id).await {
            // Best-effort: even if destroy fails we drop the binding
            // and mark Idle. The sandbox is the cache, not source of truth.
            tracing::warn!(
                session_id = %id,
                sandbox_id = %sandbox_id,
                error = %e,
                "sandbox destroy failed during evict_local; continuing",
            );
        }
    }

    // Clear the persisted sandbox_id so a coordinator restart
    // doesn't repopulate routing for a sandbox that no longer
    // exists. host_id stays so resume's snapshot affinity still
    // prefers the same host.
    let _ = state.services.meta.assign_session_sandbox(id, None).await;
    state
        .services
        .meta
        .set_session_status(id, SessionStatus::Idle)
        .await?;
    let now = Utc::now();
    state.emit(id, SessionEvent::Evicted { at: now }).await?;
    state
        .emit(
            id,
            SessionEvent::StatusChanged {
                from: SessionStatus::Active,
                to: SessionStatus::Idle,
                at: now,
            },
        )
        .await?;
    Ok(StatusCode::ACCEPTED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::sandbox::SandboxSpec;
    use engram_sandbox_process::ProcessBackend;
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

    /// Build a bare remote populated with a `<branch>` ref pointing
    /// at a commit that adds `marker_path` with `marker_content`.
    /// Returns the bare-remote tempdir; caller drops it when done.
    fn seed_bare_remote_with_branch(
        branch: &str,
        marker_path: &str,
        marker_content: &str,
    ) -> TempDir {
        let remote = TempDir::new().unwrap();
        run_host(
            &["git", "init", "--bare", "--initial-branch=main"],
            remote.path(),
        );
        // Ephemeral working clone we use to push the seed commit.
        let work = TempDir::new().unwrap();
        run_host(&["git", "init", "--initial-branch=main"], work.path());
        run_host(
            &[
                "git",
                "remote",
                "add",
                "origin",
                &format!("{}", remote.path().display()),
            ],
            work.path(),
        );
        std::fs::write(work.path().join(marker_path), marker_content).unwrap();
        run_host(&["git", "add", "-A"], work.path());
        run_host(
            &[
                "git",
                "-c",
                "user.email=t@e.local",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "seed",
            ],
            work.path(),
        );
        run_host(
            &["git", "push", "origin", &format!("HEAD:{branch}")],
            work.path(),
        );
        remote
    }

    fn process_spec() -> SandboxSpec {
        SandboxSpec {
            image: "smart-bootstrap-test".into(),
            rootfs_source: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 64 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
        }
    }

    #[tokio::test]
    async fn smart_bootstrap_clones_into_empty_workspace() {
        let remote = seed_bare_remote_with_branch(
            "engram/sessions/abc",
            "checkpoint-marker.txt",
            "from-resume",
        );
        let url = format!("{}", remote.path().display());

        let sandbox_root = TempDir::new().unwrap();
        let backend: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(sandbox_root.path()));
        let sandbox_id = backend.create(process_spec()).await.unwrap();

        smart_bootstrap_to_branch(&backend, sandbox_id, &url, "engram/sessions/abc")
            .await
            .expect("smart_bootstrap should succeed on empty workspace");

        // Marker file from the seed commit must now exist in cwd.
        let h = run_workspace(
            &backend,
            sandbox_id,
            &["sh", "-c", "cat checkpoint-marker.txt"],
        )
        .await
        .unwrap();
        assert_eq!(h.exit_status, Some(0), "marker file must exist");
        assert_eq!(
            String::from_utf8_lossy(&h.stdout).trim(),
            "from-resume",
            "workspace should be at the checkpoint branch's HEAD",
        );
    }

    #[tokio::test]
    async fn smart_bootstrap_fast_forwards_a_pre_cloned_workspace() {
        // Simulates a warm-pool image: the sandbox cwd starts as
        // a clone of the bare remote at `main` (bake SHA). Resume
        // points the checkpoint branch — `fetch + reset` brings
        // /workspace to the new state with no full re-clone.
        let remote = seed_bare_remote_with_branch("main", "base.txt", "v1");
        // Now seed a separate engram/sessions/<id> branch on the
        // same remote, with new content.
        let work_for_branch = TempDir::new().unwrap();
        run_host(
            &[
                "git",
                "clone",
                "--branch",
                "main",
                &format!("{}", remote.path().display()),
                ".",
            ],
            work_for_branch.path(),
        );
        std::fs::write(work_for_branch.path().join("agent-output.txt"), "v2").unwrap();
        run_host(&["git", "add", "-A"], work_for_branch.path());
        run_host(
            &[
                "git",
                "-c",
                "user.email=t@e.local",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "agent work",
            ],
            work_for_branch.path(),
        );
        run_host(
            &["git", "push", "origin", "HEAD:engram/sessions/xyz"],
            work_for_branch.path(),
        );

        // Pre-seed the sandbox cwd with a clone at `main` (the warm-
        // pool image's bake state).
        let warm_clone = TempDir::new().unwrap();
        run_host(
            &[
                "git",
                "clone",
                "--branch",
                "main",
                &format!("{}", remote.path().display()),
                ".",
            ],
            warm_clone.path(),
        );

        let sandbox_root = TempDir::new().unwrap();
        let backend: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(sandbox_root.path()));
        let mut spec = process_spec();
        spec.rootfs_source = Some(warm_clone.path().to_path_buf());
        let sandbox_id = backend.create(spec).await.unwrap();

        // Sanity: before bootstrap, the agent-output file isn't in
        // the warm clone (it's only on engram/sessions/xyz).
        let pre = run_workspace(
            &backend,
            sandbox_id,
            &["sh", "-c", "test -f agent-output.txt"],
        )
        .await
        .unwrap();
        assert_ne!(pre.exit_status, Some(0));

        smart_bootstrap_to_branch(
            &backend,
            sandbox_id,
            &format!("{}", remote.path().display()),
            "engram/sessions/xyz",
        )
        .await
        .expect("smart_bootstrap should fast-forward the warm clone");

        // After bootstrap the agent-output file exists; the workspace
        // is at the checkpoint branch HEAD.
        let h = run_workspace(&backend, sandbox_id, &["sh", "-c", "cat agent-output.txt"])
            .await
            .unwrap();
        assert_eq!(h.exit_status, Some(0));
        assert_eq!(String::from_utf8_lossy(&h.stdout).trim(), "v2");

        // .git survived (no re-init); the existing config is reused.
        let cfg = run_workspace(
            &backend,
            sandbox_id,
            &["git", "remote", "get-url", "origin"],
        )
        .await
        .unwrap();
        assert_eq!(cfg.exit_status, Some(0));
        assert_eq!(
            String::from_utf8_lossy(&cfg.stdout).trim(),
            format!("{}", remote.path().display())
        );
    }

    #[tokio::test]
    async fn smart_bootstrap_replaces_origin_when_url_diverges() {
        // Edge case: warm-pool image was baked with origin pointing
        // at a *different* URL (operator changed the git URL between
        // bake and now). Smart-bootstrap detects the mismatch and
        // re-initializes with the canonical URL rather than fetching
        // from the stale one.
        let real_remote =
            seed_bare_remote_with_branch("engram/sessions/foo", "from-real-remote.txt", "v1");
        let stale_remote = seed_bare_remote_with_branch("main", "stale.txt", "stale");

        // Pre-seed the sandbox cwd with a clone of the *stale* remote.
        let warm_clone = TempDir::new().unwrap();
        run_host(
            &[
                "git",
                "clone",
                "--branch",
                "main",
                &format!("{}", stale_remote.path().display()),
                ".",
            ],
            warm_clone.path(),
        );

        let sandbox_root = TempDir::new().unwrap();
        let backend: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(sandbox_root.path()));
        let mut spec = process_spec();
        spec.rootfs_source = Some(warm_clone.path().to_path_buf());
        let sandbox_id = backend.create(spec).await.unwrap();

        smart_bootstrap_to_branch(
            &backend,
            sandbox_id,
            &format!("{}", real_remote.path().display()),
            "engram/sessions/foo",
        )
        .await
        .expect("smart_bootstrap should re-init when origin URL diverges");

        // The marker from the *real* remote must be present.
        let h = run_workspace(
            &backend,
            sandbox_id,
            &["sh", "-c", "cat from-real-remote.txt"],
        )
        .await
        .unwrap();
        assert_eq!(h.exit_status, Some(0));
        assert_eq!(String::from_utf8_lossy(&h.stdout).trim(), "v1");
    }
}
