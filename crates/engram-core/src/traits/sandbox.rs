use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::StreamExt;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::error::SandboxError;
use crate::types::ids::SandboxId;
use crate::types::sandbox::{
    AgentSpec, ExecEvent, ExecHandle, ExecRequest, ExecStream, SandboxSpec,
};
use crate::types::snapshot::SnapshotMetadata;

/// Combined `AsyncRead + AsyncWrite` so trait-object types below can
/// require both — Rust's trait-object syntax only allows one
/// non-auto trait, so we need this supertrait shim.
pub trait HarnessByteStreamObj: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite + ?Sized> HarnessByteStreamObj for T {}

/// A duplex byte stream the backend hands the harness sink. Owned
/// (`'static`) so the sink can move it into a tokio task.
pub type HarnessByteStream = Pin<Box<dyn HarnessByteStreamObj + Send + Unpin + 'static>>;

/// Callback registered on a [`SandboxBackend`] that wants to expose
/// inbound harness connections. The backend invokes the sink with a
/// fresh `HarnessByteStream` whenever a guest's harness adapter
/// dials in. The sink — typically `HarnessHub::accept_via_session_lookup`
/// — drives the post-attach loop from there.
///
/// `Fn` (not `FnOnce`) so a single sink handles many connections.
pub type HarnessSink = Arc<dyn Fn(HarnessByteStream) + Send + Sync>;

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

    /// Start the long-running agent process for this sandbox (the
    /// harness adapter — Claude Code, the dev noop). Idempotent:
    /// a second call with the agent already running is a no-op.
    ///
    /// **Why agent argv is supplied here, not on `SandboxSpec`.**
    /// The warm pool reuses one `SandboxSpec` template across many
    /// sessions in a `(repo, image_version)` bucket. Per-session
    /// agent argv (carrying `session_id`, the attach token, etc.)
    /// can't ride on that template — it'd freeze at the first
    /// session's id. The caller supplies the per-session agent at
    /// checkout time.
    ///
    /// **Why this is separate from `create`.** The coordinator
    /// wires routing (e.g. `HarnessHub::bind_session`) before the
    /// agent has a chance to dial out, so an attach can resolve
    /// its target without racing against the spawn.
    ///
    /// Default impl errors with `InvalidSpec` — backends that
    /// don't yet support agents (Firecracker until
    /// `engram-bootstrap` lands) inherit it; backends that do
    /// (ProcessBackend) override.
    async fn start_agent(
        &self,
        _id: SandboxId,
        _agent: AgentSpec,
    ) -> Result<(), SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this backend doesn't support `start_agent` yet".into(),
        ))
    }

    /// Register a sink that will receive inbound harness connections
    /// (one stream per guest dial). Backends that route the harness
    /// channel through their own transport (FC's vsock UDS, future
    /// ones) call the sink for each fresh connection. Default is a
    /// no-op — `ProcessBackend` doesn't need this hook because its
    /// harness binary dials the host-agent's TCP listener directly.
    ///
    /// Idempotent in the sense that the latest registration wins;
    /// backends shouldn't expect multiple sinks. Pass before any
    /// session-create call so the first dial isn't dropped.
    fn set_harness_sink(&self, _sink: HarnessSink) {}

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

    /// IPv4 address the *host* can use to reach a TCP service running
    /// inside this sandbox's guest. Used by the `GET /sessions/:id/shell`
    /// proxy to dial `ttyd` on the guest. Returns `None` if:
    ///
    /// - the sandbox isn't running yet (no agent up to ask),
    /// - the backend has no host→guest IP routing wired (FC today),
    /// - the agent reported no eligible non-loopback address.
    ///
    /// Default returns `None` so backends that don't yet implement
    /// host→guest IP discovery (FC) inherit a clean "shell unavailable"
    /// surface.
    async fn guest_ip(&self, _id: SandboxId) -> Option<String> {
        None
    }
}
