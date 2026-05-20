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
use crate::types::ids::TemplateRef;
use crate::types::sandbox::{AgentSpec, ExecHandle, ExecRequest, ExecStream, SandboxSpec};
use crate::types::shell::ShellTunnel;
use crate::types::snapshot::SnapshotMetadata;
use crate::types::{SandboxId, SessionId};

/// ADR 0014: outcome of [`HostClient::lease_warm_sandbox`].
///
/// Distinct from `Result<Option<SandboxId>>` because the scheduler
/// treats "stale pool" differently from "no capacity" — stale
/// prompts a cache invalidation + try-next-host, no-capacity
/// passes silently.
#[derive(Clone, Debug)]
pub enum WarmLeaseOutcome {
    /// Granted a warm slot — coord follows up with
    /// [`HostClient::launch_warm_sandbox`] to push BootstrapLaunch.
    Granted(SandboxId),
    /// Host's warm pool is for an older template_ref than the
    /// coord asked about. `current_ref` is the host's most-recently-
    /// known active ref; coord uses it to lazily update its cache
    /// and re-resolve.
    Stale { current_ref: TemplateRef },
    /// Pool is currently empty for this template (refill in flight,
    /// or autoscaler target is 0). Scheduler falls through to
    /// cold-create with one tracing::warn.
    NoCapacity,
}

/// ADR 0014: one entry in the host's warm-pool inventory, returned
/// by [`HostClient::list_warm_slots`] and (via heartbeat) by
/// [`HostCapacityReport::warm_slots`].
#[derive(Clone, Debug)]
pub struct WarmSlotCount {
    pub template_ref: TemplateRef,
    /// Sandboxes currently in the free-list (ready to lease).
    pub available: u32,
    /// Target N that the autoscaler is keeping the pool at.
    pub target: u32,
}

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

    // ---- ADR 0014 warm pool ----

    /// Atomic take from this host's free-list of pre-restored
    /// microVMs for `template_ref`. Default `NoCapacity` because
    /// only the FC host-agent maintains a warm pool today;
    /// LocalHostClient (mode=all) and ProcessBackend (dev) just
    /// fall through to cold-create.
    async fn lease_warm_sandbox(
        &self,
        template_ref: TemplateRef,
    ) -> Result<WarmLeaseOutcome, SandboxError> {
        let _ = template_ref;
        Ok(WarmLeaseOutcome::NoCapacity)
    }

    /// Activate a previously-leased warm sandbox: push BootstrapLaunch
    /// to in-guest engram-bootstrap and apply the SessionEgressPolicy.
    /// The pre-restored substrate is already running by this point;
    /// this RPC is the per-session activation, not the create.
    ///
    /// Default impl errors with `NotFound` since impls that don't
    /// implement [`Self::lease_warm_sandbox`] can never have a
    /// sandbox_id that corresponds to a leased warm slot.
    ///
    /// ADR 0014 M1.12: `harness_pack_uri` lets the host swap the
    /// warm slot's harness drive to the session's chosen ext4 via
    /// `swap_harness_drive` before `start_agent` dials bootstrap.
    /// `None` skips the swap (the slot's bake-time stub stays
    /// attached — fine for sessions with no harness, or when the
    /// stub already matches).
    ///
    /// `harness_name` is the canonical name (e.g. `"claude"`) the
    /// session selected — the directory bootstrap inside the VM
    /// expects under `/run/engram/harnesses/<name>/`. Required when
    /// `harness_pack_uri` is `Some`, since the URI's last path
    /// segment (e.g. `harness-claude`) won't generally match the
    /// session's canonical name. `None` when the URI is also `None`.
    async fn launch_warm_sandbox(
        &self,
        sandbox_id: SandboxId,
        agent: AgentSpec,
        policy: SessionEgressPolicy,
        harness_pack_uri: Option<String>,
        harness_name: Option<String>,
    ) -> Result<(), SandboxError> {
        let _ = (sandbox_id, agent, policy, harness_pack_uri, harness_name);
        Err(SandboxError::NotFound)
    }

    /// Inspect this host's warm-pool inventory. Default empty; only
    /// the warm-pool-equipped host-agent populates it. Ops tooling
    /// (`engram-cli warm-pool`) and the coord scheduler's
    /// "skip-zero-slot-host" hint both consume this.
    async fn list_warm_slots(&self) -> Result<Vec<WarmSlotCount>, SandboxError> {
        Ok(Vec::new())
    }
}
