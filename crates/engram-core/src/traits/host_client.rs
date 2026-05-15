//! The coord↔host boundary trait.
//!
//! `SandboxBackend` is the VMM dimension (FC vs VZ vs Process — "what
//! kind of VM am I"). `HostClient` is the transport dimension ("where
//! am I"). Splitting the two means the coord never imports
//! `SandboxBackend` directly — it talks to a `HostClient`, which
//! either composes a local backend (in-process) or sends RPCs over the
//! WS (remote). Two impls today:
//!
//! - `LocalHostClient` in `engram-host-agent::host_client` — wraps an
//!   `Arc<dyn SandboxBackend>` + an `Arc<HarnessHub>`. Used in
//!   `--mode=all` (coord constructs one inline) and inside the
//!   host-agent itself (what its WS server serves over the wire).
//! - `RemoteHostClient` in `engram-protocol::client` — wraps a
//!   `ConnectedHost` (WS). Every method is a unary RPC.
//!
//! The trait surface unions what used to live separately on
//! `SandboxBackend`'s coord-facing methods and on `HarnessHub`'s
//! direct in-process API. `snapshot_path_for` and `set_harness_sink`
//! stay on `SandboxBackend` only — they're host-local concepts that
//! don't cross the wire.

use async_trait::async_trait;

use crate::error::SandboxError;
use crate::traits::sandbox::{HarnessDial, HarnessSink};
use crate::types::egress::SessionEgressPolicy;
use crate::types::sandbox::{AgentSpec, ExecHandle, ExecRequest, ExecStream, SandboxSpec};
use crate::types::snapshot::SnapshotMetadata;
use crate::types::{SandboxId, SessionId};

#[async_trait]
pub trait HostClient: Send + Sync {
    // ---- sandbox lifecycle ----
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError>;
    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError>;
    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError>;

    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError>;
    async fn exec(&self, id: SandboxId, cmd: ExecRequest) -> Result<ExecHandle, SandboxError> {
        use futures::stream::StreamExt;
        let mut stream = self.exec_stream(id, cmd).await?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_status = None;
        while let Some(event) = stream.events.next().await {
            match event {
                crate::types::sandbox::ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
                crate::types::sandbox::ExecEvent::Stderr(b) => stderr.extend_from_slice(&b),
                crate::types::sandbox::ExecEvent::Exit(s) => {
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

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError>;
    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError>;

    async fn start_agent(&self, id: SandboxId, agent: AgentSpec) -> Result<(), SandboxError>;
    async fn notify_session_policy(&self, policy: SessionEgressPolicy) -> Result<(), SandboxError>;
    async fn guest_ip(&self, id: SandboxId) -> Option<String>;

    // ---- harness routing ----
    /// Tell this host that an upcoming harness connection identifying
    /// itself with `session_id` should be routed to `sandbox_id`.
    /// Errors are infallible locally (the local hub just inserts into
    /// a HashMap); remote impls swallow transport failures into a
    /// warning log because the harness can still attach via the
    /// session-id lookup path on its end.
    async fn bind_session(&self, session_id: SessionId, sandbox_id: SandboxId);

    /// Drop the session→sandbox binding.
    async fn unbind_session(&self, session_id: SessionId);

    /// Forward a user prompt to the attached harness for `sandbox_id`.
    /// `SandboxError::NotFound` if no harness is bound (call
    /// `ensure_active` upstream to auto-resume). Other errors come from
    /// the underlying writer dropping or the harness disconnecting
    /// mid-send.
    async fn send_prompt(&self, sandbox_id: SandboxId, text: String) -> Result<(), SandboxError>;

    /// ADR 0013 + ADR 0011 follow-up #3: pin a sandbox against idle
    /// eviction while a shell WebSocket is open. The local hub is the
    /// only source of truth for "is a shell attached to this sandbox?"
    /// — the in-proc `LocalHostClient` reaches its hub directly;
    /// remote impls route to the host that owns the harness session.
    /// Reference-counted in the hub so a future second client doesn't
    /// decrement to zero prematurely.
    async fn acquire_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError>;

    /// Release a `acquire_shell` reference. Called on shell bridge
    /// exit (success or error). Symmetric with `acquire_shell`; the
    /// hub silently swallows underflow rather than erroring so a buggy
    /// caller can't poison the count.
    async fn release_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError>;

    /// How a harness process inside this host's sandboxes dials back
    /// to the harness channel. Static per-host capability — drives
    /// the argv shape `resolve_harness` builds. Default `Vsock`
    /// because FC + VZ are the production backends.
    fn harness_dial(&self) -> HarnessDial {
        HarnessDial::Vsock
    }

    /// Register a harness sink on this host's local VMM backend. Only
    /// meaningful for in-process impls — `LocalHostClient` forwards
    /// the closure straight to its inner `SandboxBackend`. Remote impls
    /// no-op: the host on the other end of the wire wires its own
    /// local sink internally, and the closure (capturing coord-side
    /// state) wouldn't serialize anyway.
    fn set_harness_sink(&self, _sink: HarnessSink) {}
}
