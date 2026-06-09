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
use crate::traits::sandbox::{ForgeSink, HarnessDial, HarnessSink, UploadSink};
use crate::types::cow_state::{CowState, CowStateRecord};
use crate::types::egress::SessionEgressPolicy;
use crate::types::sandbox::{AgentSpec, ExecHandle, ExecRequest, ExecStream, SandboxSpec};
use crate::types::shell::ShellTunnel;
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
    /// ADR 0014 issue #1/#2: commit a snapshot whose post-snapshot
    /// pipeline has fully succeeded. See `SandboxBackend::commit_snapshot`
    /// for the contract. Default impl returns Ok so HostClients backed by
    /// backends that don't need a commit phase (Process, VZ-dev) work
    /// unchanged.
    async fn commit_snapshot(&self, _id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    /// ADR 0014 issue #1/#2: abort a snapshot whose downstream pipeline
    /// failed. Idempotent. See `SandboxBackend::abort_snapshot`.
    async fn abort_snapshot(&self, _id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }
    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError>;

    /// ADR 0020 P1: boot the image to agentd-ready, snapshot it, tear
    /// the capture VM down, and return the portable snapshot metadata.
    /// Called by the coord during `POST /api/enabled-images` to produce
    /// the per-image base snapshot `create_session` restores from.
    /// Default errors so mocks / non-FC hosts opt out; the local +
    /// gRPC clients delegate to the backend's `build_base_snapshot`.
    async fn build_base_snapshot(
        &self,
        _spec: SandboxSpec,
    ) -> Result<SnapshotMetadata, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this host doesn't support `build_base_snapshot`".into(),
        ))
    }

    /// ADR 0020 P1: restore a per-image base snapshot for a session
    /// and inject the per-session env. Called by `create_session` on a
    /// base-snapshot hit. ADR 0021 P1.5 retired the option-D
    /// substrate-swap stage that used to follow the restore — the
    /// harness lives in the rootfs now. Default errors so mocks /
    /// non-FC hosts opt out.
    async fn restore_base_for_session(
        &self,
        _metadata: SnapshotMetadata,
        _session_env: std::collections::HashMap<String, String>,
    ) -> Result<SandboxId, SandboxError> {
        Err(SandboxError::InvalidSpec(
            "this host doesn't support `restore_base_for_session`".into(),
        ))
    }

    /// ADR 0013: bundle the egress policy with the agent spawn so the
    /// host applies the policy to its egress proxy registry BEFORE
    /// starting the agent process. Atomic by construction —
    /// eliminates the WS-era frame-ordering invariant that the
    /// stateless transport can't honour. Local impl applies in-proc;
    /// the gRPC impl ships both fields in a single `StartAgent` RPC.
    async fn start_agent(
        &self,
        id: SandboxId,
        agent: AgentSpec,
        policy: SessionEgressPolicy,
    ) -> Result<(), SandboxError>;

    /// Apply an egress policy to the host's local proxy registry
    /// without spawning an agent. The companion to `start_agent`'s
    /// bundled form, for sessions that don't carry a harness but
    /// can still emit outbound traffic via raw `/exec`. Idempotent:
    /// the host's egress registry keys on
    /// `(session_id, sandbox_id, guest_ip)` and upserts.
    async fn apply_egress_policy(&self, policy: SessionEgressPolicy) -> Result<(), SandboxError>;

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

    /// ADR 0030: operator interrupt — stop the in-flight run on the
    /// attached harness for `sandbox_id` while keeping the session
    /// alive. The harness SIGINTs its current child and returns to Idle;
    /// the next prompt resumes the same conversation via `--resume`.
    /// `SandboxError::NotFound` if no harness is bound. Default is a
    /// no-op for impls without a real harness (test fakes); the
    /// `HostRegistry`, gRPC client, and `LocalHostClient` override it.
    async fn interrupt(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }

    /// ADR 0045 Phase F: freeze the running microVM for `sandbox_id`
    /// *in place* — pause its vCPUs without snapshotting, destroying, or
    /// changing session state. An admin affordance to drive + observe
    /// the pause/flush path (and the test surface for the live-migration
    /// work). Default no-op for harness-less fakes; the `HostRegistry`,
    /// gRPC client, and `LocalHostClient` override it to reach the
    /// backend's [`crate::traits::SandboxBackend::pause`].
    async fn pause(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }

    /// ADR 0045 Phase F: unfreeze a [`Self::pause`]d microVM — resume
    /// its vCPUs in place. Symmetric with `pause`; same overrides.
    async fn resume(&self, _sandbox_id: SandboxId) -> Result<(), SandboxError> {
        Ok(())
    }

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

    /// ADR 0014 issue #6: open a bidi shell tunnel to the in-guest
    /// `ttyd` for `sandbox_id`. The returned [`ShellTunnel`] is a
    /// pair of mpsc channels:
    /// - `outbound`: caller (coord) sends WS frames here; the
    ///   implementation forwards each frame to ttyd inside the guest.
    /// - `inbound`: implementation pushes WS frames received from
    ///   ttyd here; caller drains and forwards to the browser.
    ///
    /// On either channel closing (browser disconnect, ttyd exit,
    /// transport error) the implementation drops both halves and
    /// frees its proxy task; idempotent. The first-frame `sandbox_id`
    /// convention on the wire (gRPC ProxyShellFrame) lives in the
    /// gRPC client/server only — this trait surface takes the
    /// `sandbox_id` directly so trait callers don't have to embed it
    /// in the frame stream.
    ///
    /// Default impl errors with `NotFound`: only host-agent
    /// implementations actually proxy shells. The Local impl in the
    /// host-agent opens a WebSocket to `ws://<guest_ip>:7681/ws`
    /// inside the right network namespace (per
    /// [`SandboxBackend::netns_name_for`]) and bridges; the gRPC
    /// client impl opens a gRPC bidi stream and bridges the channels
    /// with the wire frames.
    async fn proxy_shell(&self, sandbox_id: SandboxId) -> Result<ShellTunnel, SandboxError> {
        let _ = sandbox_id;
        Err(SandboxError::NotFound)
    }

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

    /// ADR 0023: register a forge sink on this host's local backend.
    /// Same in-process-only semantics as [`set_harness_sink`](Self::set_harness_sink)
    /// — `LocalHostClient` forwards to its inner `SandboxBackend`;
    /// remote impls no-op.
    fn set_forge_sink(&self, _sink: ForgeSink) {}

    /// ADR 0026: register an artifact-upload sink on this host's local
    /// backend. Same in-process-only semantics as
    /// [`set_forge_sink`](Self::set_forge_sink) — `LocalHostClient`
    /// forwards to its inner `SandboxBackend`; remote impls no-op.
    fn set_upload_sink(&self, _sink: UploadSink) {}

    /// ADR 0016 Phase A: per-sandbox COW diagnostic snapshot.
    /// `None` if this host doesn't have a chunk-tracked view of
    /// the sandbox (backend not NBD-attached, or the
    /// `sandbox_id` isn't bound here). The caller — `cow_state`
    /// fan-out at coord — treats `Ok(None)` and `Err(NotFound)`
    /// uniformly. Default returns `None` so `HostClient` impls
    /// that don't forward this can compile without changes.
    async fn cow_state(&self, _id: SandboxId) -> Result<Option<CowState>, SandboxError> {
        Ok(None)
    }

    /// ADR 0016 Phase A: bulk variant. One record per chunk-tracked
    /// sandbox this host owns. Cheaper than `list() + N ×
    /// cow_state()` over the wire — single RPC, single map walk on
    /// the host. Default empty so non-host-agent impls (mocks,
    /// in-proc test glue) compile.
    async fn cow_state_all(&self) -> Result<Vec<CowStateRecord>, SandboxError> {
        Ok(Vec::new())
    }

    /// ADR 0016 Phase B commit 4a — admin trigger for the
    /// FlushScheduler primitive. Coord's
    /// `POST /api/admin/sessions/:id/flush-now` calls this on the
    /// session's bound host to force a flush + manifest publish.
    /// Returns the new `ManifestRef` if any chunks were drained;
    /// `None` if the sandbox isn't chunk-tracked or no dirty bytes
    /// were buffered.
    ///
    /// Default `Ok(None)` so mocks and the warm-start scheduler-less
    /// pre-Phase-B impls compile unchanged.
    async fn flush_sandbox(
        &self,
        _id: SandboxId,
    ) -> Result<Option<crate::types::manifest::ManifestRef>, SandboxError> {
        Ok(None)
    }
}
