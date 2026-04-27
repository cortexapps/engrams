use std::path::{Path, PathBuf};

use async_trait::async_trait;
use futures::stream::StreamExt;

use crate::error::SandboxError;
use crate::types::ids::SandboxId;
use crate::types::sandbox::{ExecEvent, ExecHandle, ExecRequest, ExecStream, SandboxSpec};
use crate::types::snapshot::SnapshotMetadata;

/// VM lifecycle seam. Production implementation: `engram-sandbox-firecracker`
/// (Firecracker over its HTTP-over-Unix-socket API). Dev implementation:
/// `engram-sandbox-process` (host subprocesses, no isolation).
///
/// `exec_stream` is the primary exec method — it returns immediately
/// with a stream of [`ExecEvent`]s so callers can show output in real
/// time (and so multiple subscribers can observe one command running).
/// `exec` is provided with a default impl that drains the stream into
/// an in-memory [`ExecHandle`]; it's a convenience for short commands
/// and unit tests. Backend implementations only need to provide
/// `exec_stream`.
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError>;

    /// Run a command in the sandbox and return a stream of stdout/stderr
    /// chunks ending with a single [`ExecEvent::Exit`]. Terminating the
    /// stream early (dropping it) does NOT necessarily kill the
    /// underlying process — backends are free to detach and let it run
    /// to completion. Callers that need cancellation should hold the
    /// stream until exit or use a separate kill API (future work).
    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError>;

    /// Convenience wrapper that runs the command to completion and
    /// returns buffered stdout/stderr/exit-status. Default impl drains
    /// `exec_stream`. Don't call this for long-running commands —
    /// stdout/stderr live in memory until the process exits.
    async fn exec(&self, id: SandboxId, cmd: ExecRequest) -> Result<ExecHandle, SandboxError> {
        let mut stream = self.exec_stream(id, cmd).await?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_status = None;
        while let Some(event) = stream.events.next().await {
            match event {
                ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
                ExecEvent::Stderr(b) => stderr.extend_from_slice(&b),
                ExecEvent::Exit(s) => {
                    exit_status = s;
                    break;
                }
            }
        }
        Ok(ExecHandle {
            sandbox_id: stream.sandbox_id,
            exec_id: stream.exec_id,
            stdout,
            stderr,
            exit_status,
        })
    }

    async fn snapshot(&self, id: SandboxId, dest: &Path) -> Result<SnapshotMetadata, SandboxError>;
    async fn restore(&self, src: PathBuf) -> Result<SandboxId, SandboxError>;
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError>;
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError>;
}
