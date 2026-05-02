//! Materialize a session's `WorkspaceSpec` into the running sandbox.
//!
//! Runs *after* `backend.create(spec)` (the rootfs is up; for warm-pool
//! slots it's also already booted) and *before* `backend.start_agent()`
//! so the agent never observes a half-populated workspace. Materialise
//! is per-session and per-sandbox; never bake workspace state into the
//! warm-pool spec — pool slots are anonymous and reused.

use std::time::Duration;

use engram_core::error::SandboxError;
use engram_core::traits::SandboxBackend;
use engram_core::types::sandbox::ExecRequest;
use engram_core::types::session::WorkspaceSpec;
use engram_core::SandboxId;

/// Errors raised by [`materialize`]. The variants map cleanly to API
/// failure shapes (Conflict / Internal); the API layer wraps them.
#[derive(Debug)]
pub enum WorkspaceError {
    Exec(SandboxError),
    Clone {
        url: String,
        code: Option<i32>,
        stderr: String,
    },
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exec(e) => write!(
                f,
                "backend exec failed during workspace materialization: {e}"
            ),
            Self::Clone { url, code, stderr } => {
                write!(f, "git clone of `{url}` failed (exit {code:?}): {stderr}")
            }
        }
    }
}

impl std::error::Error for WorkspaceError {}

impl From<SandboxError> for WorkspaceError {
    fn from(e: SandboxError) -> Self {
        Self::Exec(e)
    }
}

/// The guest path where Git / LocalMount workspaces land. Hard-coded
/// for v1 — future iterations may make it configurable per image.
pub const DEFAULT_GUEST_WORKSPACE: &str = "/workspace";

/// Bring up the workspace for a freshly-created sandbox, observing
/// the variant of `WorkspaceSpec`:
///
/// - `Empty`     → no-op. Whatever the image baked is what's there.
/// - `Git`       → `git clone <url> -b <branch> --single-branch
///   /workspace` via `backend.exec` so the rootfs acquires a real
///   `.git` dir. Read-only flag affects checkpoint behavior, not the
///   clone.
/// - `LocalMount`→ no-op here. The host-to-guest share is wired into
///   `SandboxSpec.mounts` at create time; the backend has already
///   mounted it.
pub async fn materialize(
    backend: &dyn SandboxBackend,
    sandbox_id: SandboxId,
    spec: &WorkspaceSpec,
) -> Result<(), WorkspaceError> {
    match spec {
        WorkspaceSpec::Empty => Ok(()),
        WorkspaceSpec::LocalMount { .. } => Ok(()),
        WorkspaceSpec::Git { url, branch, .. } => {
            clone_into_sandbox(backend, sandbox_id, url, branch).await
        }
    }
}

async fn clone_into_sandbox(
    backend: &dyn SandboxBackend,
    sandbox_id: SandboxId,
    url: &str,
    branch: &str,
) -> Result<(), WorkspaceError> {
    // `--single-branch` keeps only the requested branch's history —
    // future fetches can deepen if checkpoint logic needs more, but
    // every saved second at session-create time matters more than
    // the disk savings. Hardcode `/workspace` as the guest path; the
    // checkpoint runner reads the same constant.
    let req = ExecRequest {
        command: vec![
            "git".into(),
            "clone".into(),
            "--single-branch".into(),
            "--branch".into(),
            branch.into(),
            url.into(),
            DEFAULT_GUEST_WORKSPACE.into(),
        ],
        stdin: None,
        env: Default::default(),
        workdir: None,
        timeout: Some(Duration::from_secs(300)),
    };
    let handle = backend.exec(sandbox_id, req).await?;
    match handle.exit_status {
        Some(0) => Ok(()),
        code => {
            let stderr = String::from_utf8_lossy(&handle.stderr).into_owned();
            Err(WorkspaceError::Clone {
                url: url.to_string(),
                code,
                stderr,
            })
        }
    }
}
