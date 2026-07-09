//! `LocalHostClient` — the in-process composition of a
//! [`SandboxBackend`] and a [`HarnessHub`] that together satisfy the
//! [`HostClient`] trait.
//!
//! Used in two places:
//!
//! - In `--mode=all` the coord constructs one of these (with a local
//!   `PooledBackend` + a `HarnessHub` whose `EventSink` writes to
//!   `state.emit`) and registers it in `HostRegistry`.
//! - Inside the host-agent itself, this is what
//!   `engram-protocol::server` serves over the WS — so the coord on
//!   the other end of a `RemoteHostClient` is talking to one of these.
//!
//! All sandbox methods are pure delegation to the inner `SandboxBackend`.
//! The harness methods (`bind_session`, `unbind_session`,
//! `send_prompt`) delegate to the `HarnessHub`, mapping its
//! `HarnessError` into `SandboxError` so the trait surface stays
//! uniform.

use std::sync::Arc;

use async_trait::async_trait;

use engram_core::error::SandboxError;
use engram_core::traits::{HarnessDial, HarnessSink, HostClient, SandboxBackend, SessionFence};
use engram_core::types::cow_state::{CowState, CowStateRecord};
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::image::WarmConfig;
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::types::{SandboxId, SessionId};

use crate::harness::{HarnessError, HarnessHub};

#[derive(Clone)]
pub struct LocalHostClient {
    sandbox: Arc<dyn SandboxBackend>,
    harness_hub: Arc<HarnessHub>,
}

impl LocalHostClient {
    pub fn new(sandbox: Arc<dyn SandboxBackend>, harness_hub: Arc<HarnessHub>) -> Self {
        Self {
            sandbox,
            harness_hub,
        }
    }

    /// Convenience constructor for callers that don't need to route
    /// harness events through a shared hub (tests, in-process glue
    /// where the harness path isn't exercised). Builds a `HarnessHub`
    /// with a no-op `EventSink` so the trait surface is satisfied.
    pub fn with_noop_hub(sandbox: Arc<dyn SandboxBackend>) -> Self {
        // Ephemeral bindings dir: callers of this constructor never
        // exercise the attach path (tests / in-process glue), and an
        // ephemeral dir keeps the ADR 0073 validation code identical
        // rather than special-cased.
        let dir =
            std::env::temp_dir().join(format!("engram-noop-hub-bindings-{}", uuid::Uuid::new_v4()));
        let bindings = crate::bindings::BindingStore::open(dir)
            .expect("open ephemeral binding store for noop hub");
        let hub = Arc::new(HarnessHub::new(
            crate::harness::event_sink_to(|_, _, _| async {}),
            bindings,
        ));
        Self::new(sandbox, hub)
    }

    /// Borrow the inner sandbox backend. Used by callers that legitimately
    /// need the local `SandboxBackend` surface (the host-agent's
    /// startup live-attach pass, in-process snapshot path helpers).
    /// Coord-side code goes through the `HostClient` trait instead.
    pub fn sandbox(&self) -> &Arc<dyn SandboxBackend> {
        &self.sandbox
    }

    pub fn harness_hub(&self) -> &Arc<HarnessHub> {
        &self.harness_hub
    }
}

#[async_trait]
impl HostClient for LocalHostClient {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        self.sandbox.create(spec).await
    }

    // ADR 0079: the `fence` params below are checked by the gRPC
    // server's `check_session_epoch` gate BEFORE it delegates here —
    // the local composition is behind the fence, not the fence itself
    // (and in --mode=all there is no second writer to fence out).
    async fn destroy(&self, id: SandboxId, _fence: SessionFence) -> Result<(), SandboxError> {
        // ADR 0073 phase 4: no shell pin to clear — the pin is a PG
        // column stamped by the coordinator's relay and lapses by
        // itself (the issue #219 leak class is gone with the refcount).
        self.sandbox.destroy(id).await
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        self.sandbox.list().await
    }

    async fn probe_sandbox(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::sandbox::SandboxProbe, SandboxError> {
        self.sandbox.probe_sandbox(id).await
    }

    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        self.sandbox.exec_stream(id, cmd).await
    }

    async fn snapshot(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        self.sandbox.snapshot(id).await
    }

    async fn snapshot_begin(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<engram_core::types::SnapshotId, SandboxError> {
        // ADR 0052 Phase 2 (clean-idle-shutdown). The two-phase
        // `snapshot_begin` is the IDLE-eviction capture entry: the
        // coordinator only drives begin/wait/commit for
        // `target_state == Idle` (live teleport goes through
        // `migration_capture`; periodic + manual checkpoints through
        // `snapshot`). So this is exactly where "gate strictly to the idle
        // path" lands — no `CheckpointReason` plumbing needed.
        //
        // Before pausing+capturing, gracefully stop the in-guest `claude`
        // so the snapshot holds NO live agent: `drain` sends
        // `Shutdown { grace }` (closing claude's stdin → it drains the
        // in-flight turn to a final `result` and exits 0, then the harness
        // exits) and waits for the harness vsock to drop. On resume agentd
        // respawns the harness, which `--resume`s into the same on-disk
        // session (decision 1: respawn-with-resume). Best-effort: an
        // un-drained harness still gets captured (and reattached on
        // resume) — we never block an eviction on a stuck agent.
        if !self
            .harness_hub
            .drain(id, crate::harness::IDLE_DRAIN_GRACE_SECS)
            .await
        {
            tracing::warn!(
                sandbox_id = %id,
                "idle eviction: harness did not cleanly drain before capture",
            );
        }
        self.sandbox.snapshot_begin(id).await
    }

    async fn snapshot_wait(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<SnapshotMetadata, SandboxError> {
        self.sandbox.snapshot_wait(id).await
    }

    async fn migration_presetup(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<engram_core::types::snapshot::MigrationPresetupOut, SandboxError> {
        self.sandbox.migration_presetup(id).await
    }

    async fn migration_capture_postcopy(
        &self,
        id: SandboxId,
        export_id: &str,
        _fence: SessionFence,
    ) -> Result<engram_core::types::snapshot::PostCopyCaptureOut, SandboxError> {
        self.sandbox.migration_capture_postcopy(id, export_id).await
    }

    async fn migration_drain_wait(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::snapshot::DrainOutcome, SandboxError> {
        self.sandbox.migration_drain_wait(id).await
    }

    async fn migration_capture(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<engram_core::types::snapshot::MigrationCaptureOut, SandboxError> {
        self.sandbox.migration_capture(id).await
    }

    async fn migration_fetch(
        &self,
        export_id: &str,
        items: Vec<engram_core::types::snapshot::MigrationItem>,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<engram_core::types::snapshot::MigrationFrame, SandboxError>,
        >,
        SandboxError,
    > {
        self.sandbox.migration_fetch(export_id, items).await
    }

    async fn migration_commit(
        &self,
        id: SandboxId,
        export_id: &str,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        self.sandbox.migration_commit(id, export_id).await
    }

    async fn migration_abort(
        &self,
        id: SandboxId,
        export_id: &str,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        self.sandbox.migration_abort(id, export_id).await
    }

    async fn commit_snapshot(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        self.sandbox.commit_snapshot(id).await
    }

    async fn abort_snapshot(
        &self,
        id: SandboxId,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        self.sandbox.abort_snapshot(id).await
    }

    async fn restore(
        &self,
        metadata: SnapshotMetadata,
        _fence: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        self.sandbox.restore(metadata).await
    }

    async fn build_base_snapshot(
        &self,
        spec: SandboxSpec,
        warm: Option<WarmConfig>,
        capture_env: std::collections::HashMap<String, String>,
        capture_egress: Option<SessionEgressPolicy>,
        progress: tokio::sync::mpsc::Sender<engram_core::types::CaptureProgress>,
    ) -> Result<SnapshotMetadata, SandboxError> {
        self.sandbox
            .build_base_snapshot(spec, warm, capture_env, capture_egress, progress)
            .await
    }

    async fn materialize_image(
        &self,
        image_uri: &str,
        platform_os: &str,
        platform_arch: &str,
        registry_auth: Option<engram_core::types::registry::ResolvedRegistryAuth>,
        progress: tokio::sync::mpsc::Sender<engram_core::types::MaterializeProgress>,
    ) -> Result<engram_core::types::MaterializedImage, SandboxError> {
        self.sandbox
            .materialize_image(
                image_uri,
                platform_os,
                platform_arch,
                registry_auth,
                progress,
            )
            .await
    }

    async fn restore_base_for_session(
        &self,
        metadata: SnapshotMetadata,
        session_env: std::collections::HashMap<String, String>,
        selected_mounts: Vec<engram_core::types::sandbox::AuxRoDrive>,
        _fence: SessionFence,
    ) -> Result<SandboxId, SandboxError> {
        self.sandbox
            .restore_base_for_session(metadata, session_env, selected_mounts)
            .await
    }

    async fn start_agent(
        &self,
        id: SandboxId,
        agent: AgentSpec,
        policy: SessionEgressPolicy,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        // ADR 0073: persist the binding record BEFORE the harness can
        // dial (the spawn below), so the first attach validates instead
        // of bouncing UnknownBinding. Monotonic: a stale caller's bind
        // is refused, which is the fence working as designed.
        if agent.binding_epoch > 0 && !agent.argv.is_empty() {
            if let Err(e) =
                self.harness_hub
                    .bind_session(policy.session_id, id, agent.binding_epoch)
            {
                tracing::warn!(
                    session_id = %policy.session_id,
                    sandbox_id = %id,
                    binding_epoch = agent.binding_epoch,
                    error = %e,
                    "start_agent bind refused (stale epoch) — not spawning a superseded harness",
                );
                return Err(SandboxError::InvalidSpec(
                    "binding superseded by a newer generation".into(),
                ));
            }
        }
        // ADR 0013 atomicity: apply the policy *first* so the egress
        // proxy registry is live before the agent process spawns and
        // tries to dial out. Both ops touch the inner SandboxBackend
        // in-proc; local memory ordering carries the invariant.
        self.sandbox.notify_session_policy(policy).await?;
        self.sandbox.start_agent(id, agent).await
    }

    async fn apply_egress_policy(&self, policy: SessionEgressPolicy) -> Result<(), SandboxError> {
        self.sandbox.notify_session_policy(policy).await
    }

    async fn guest_ip(&self, id: SandboxId) -> Option<std::net::Ipv4Addr> {
        self.sandbox
            .guest_endpoints(id)
            .await
            .map(|ep| ep.egress_identity)
    }

    async fn bind_session(&self, session_id: SessionId, sandbox_id: SandboxId, binding_epoch: u64) {
        if let Err(e) = self
            .harness_hub
            .bind_session(session_id, sandbox_id, binding_epoch)
        {
            // A refused bind means a NEWER generation already owns the
            // record (monotonicity) — the caller is stale, and the
            // correct outcome is exactly "this bind does not take".
            tracing::warn!(%session_id, %sandbox_id, binding_epoch, error = %e, "bind_session refused");
        }
    }

    async fn unbind_session(&self, session_id: SessionId) {
        self.harness_hub.unbind_session(session_id);
    }

    async fn send_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
        text: String,
    ) -> Result<(), SandboxError> {
        self.harness_hub
            .send_prompt(sandbox_id, prompt_id, text)
            .await
            .map_err(harness_err_to_sandbox)
    }

    async fn edit_queued_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
        text: String,
    ) -> Result<(), SandboxError> {
        self.harness_hub
            .edit_queued_prompt(sandbox_id, prompt_id, text)
            .await
            .map_err(harness_err_to_sandbox)
    }

    async fn dequeue_queued_prompt(
        &self,
        sandbox_id: SandboxId,
        prompt_id: String,
    ) -> Result<(), SandboxError> {
        self.harness_hub
            .dequeue_queued_prompt(sandbox_id, prompt_id)
            .await
            .map_err(harness_err_to_sandbox)
    }

    async fn answer_question(
        &self,
        sandbox_id: SandboxId,
        tool_call_id: String,
        answers: std::collections::BTreeMap<String, Vec<String>>,
    ) -> Result<(), SandboxError> {
        self.harness_hub
            .answer_question(sandbox_id, tool_call_id, answers)
            .await
            .map_err(harness_err_to_sandbox)
    }

    async fn interrupt(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.harness_hub
            .interrupt(sandbox_id)
            .await
            .map_err(harness_err_to_sandbox)
    }

    // ADR 0045 Phase F: freeze/unfreeze the microVM in place — pure
    // delegation to the inner backend (no harness involvement).
    async fn pause(&self, sandbox_id: SandboxId, _fence: SessionFence) -> Result<(), SandboxError> {
        self.sandbox.pause(sandbox_id).await
    }

    async fn resume(
        &self,
        sandbox_id: SandboxId,
        _fence: SessionFence,
    ) -> Result<(), SandboxError> {
        self.sandbox.resume(sandbox_id).await
    }

    async fn proxy_shell(
        &self,
        sandbox_id: SandboxId,
    ) -> Result<engram_core::types::shell::ShellTunnel, SandboxError> {
        // ADR 0014 issue #6 + ADR 0066: bridge WS frames through a
        // ShellTunnel pair, reaching ttyd via the vsock relay when the
        // backend has one (FC; VZ after Phase 2) or a direct dial_ip
        // dial otherwise (Process; VZ pre-Phase-2) — see below.
        //
        // start_shell() asks the in-VM agentd to ensure ttyd is up
        // and accepting on its port before we attempt the dial. The
        // agentd probes a local TCP connect before replying, so
        // when we get here we're guaranteed a listener exists — no
        // more racing the warm-restore against the snapshot's init
        // script (prod session 73fe33a3 on 2026-05-20 saw the host
        // dial 48s after lease and still hit Connection refused).
        let port = self.sandbox.start_shell(sandbox_id).await?;
        let (tunnel, ends) = engram_core::types::shell::ShellTunnel::pair();
        // ADR 0066: reach ttyd via the in-guest agentd relay (FC; VZ after its
        // Phase 2 real-vsock migration) — the ttyd WebSocket handshake + frames
        // ride the relay stream to the guest's `127.0.0.1:port`. Backends without
        // a vsock relay (Process; VZ pre-Phase-2) return `None` and fall back to
        // a direct dial_ip WebSocket dial. Only FC ever had a per-VM netns, and
        // FC now always takes the relay, so the direct path is netns-free.
        match self
            .sandbox
            .open_guest_stream(sandbox_id, engram_harness_proto::PROXY_PORT_VSOCK_PORT)
            .await?
        {
            Some(stream) => {
                crate::proxy_shell::open_shell_tunnel_via_relay(stream, port, ends).await?
            }
            None => {
                let dial_ip = self
                    .sandbox
                    .guest_endpoints(sandbox_id)
                    .await
                    .map(|ep| ep.dial_ip)
                    .ok_or_else(|| {
                        SandboxError::Vm("proxy_shell: guest_endpoints unavailable".into())
                    })?;
                crate::proxy_shell::open_shell_tunnel_at(dial_ip.to_string(), port, ends).await?;
            }
        }
        Ok(tunnel)
    }

    async fn proxy_port(
        &self,
        sandbox_id: SandboxId,
        port: u16,
    ) -> Result<engram_core::types::port::PortTunnel, SandboxError> {
        let (tunnel, ends) = engram_core::types::port::PortTunnel::pair();
        // ADR 0066: FC (and, after its Phase 2 migration, VZ) reach the dev
        // server through the in-guest agentd relay, which dials the guest's own
        // `127.0.0.1` — reaching loopback-bound dev servers (Vite, the Tilt UI,
        // `next dev`) that a direct dial_ip dial can't. Backends with no vsock
        // relay (Process; VZ until Phase 2) return `None` from
        // `open_guest_stream`, and we dial the guest's reachable IP directly:
        // Process => `127.0.0.1` (agentd is a host subprocess); VZ => the in-VM
        // eth0 IP. Only FC ever had a per-VM netns, and FC now always takes the
        // vsock path, so the direct path is netns-free.
        match self
            .sandbox
            .open_guest_stream(sandbox_id, engram_harness_proto::PROXY_PORT_VSOCK_PORT)
            .await?
        {
            Some(stream) => crate::proxy_port::open_vsock_tunnel_at(stream, port, ends).await?,
            None => {
                let dial_ip = self
                    .sandbox
                    .guest_endpoints(sandbox_id)
                    .await
                    .map(|ep| ep.dial_ip)
                    .ok_or_else(|| {
                        SandboxError::Vm("proxy_port: guest_endpoints unavailable".into())
                    })?;
                crate::proxy_port::open_tcp_tunnel_at(dial_ip.to_string(), port, ends).await?;
            }
        }
        Ok(tunnel)
    }

    async fn start_browser(
        &self,
        sandbox_id: SandboxId,
    ) -> Result<engram_core::traits::sandbox::BrowserStart, SandboxError> {
        // ADR 0065: bring up the in-guest browser stack (Xvfb + x11vnc +
        // headful chromium with the CDP debug port) and return the VNC port
        // (+ agentd's optional chromium-CDP liveness warning, issue #569).
        // The orchestrator reaches x11vnc :5900 (and CDP :9222) over the
        // ADR-0066 vsock port relay — the guest binds loopback, agentd dials it.
        self.sandbox.start_browser(sandbox_id).await
    }

    async fn stop_browser(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        // ADR 0065: forward to the inner backend. Teardown is at the snapshot /
        // idle-eviction boundary (the browser is ephemeral, never snapshotted).
        self.sandbox.stop_browser(sandbox_id).await
    }

    async fn start_ide(&self, sandbox_id: SandboxId) -> Result<u16, SandboxError> {
        // ADR 0081: bring up the in-guest IDE (code-server) and return its
        // loopback HTTP port. The orchestrator reaches it over the ADR-0066
        // vsock port relay — the guest binds loopback, agentd dials it.
        self.sandbox.start_ide(sandbox_id).await
    }

    async fn stop_ide(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        // ADR 0081: forward to the inner backend. Teardown is at the snapshot /
        // idle-eviction boundary (code-server is ephemeral, never snapshotted).
        self.sandbox.stop_ide(sandbox_id).await
    }

    fn harness_dial(&self) -> HarnessDial {
        self.sandbox.harness_dial()
    }

    fn set_harness_sink(&self, sink: HarnessSink) {
        self.sandbox.set_harness_sink(sink);
    }

    fn set_forge_sink(&self, sink: engram_core::traits::ForgeSink) {
        self.sandbox.set_forge_sink(sink);
    }

    fn set_upload_sink(&self, sink: engram_core::traits::UploadSink) {
        self.sandbox.set_upload_sink(sink);
    }

    async fn cow_state(&self, id: SandboxId) -> Result<Option<CowState>, SandboxError> {
        Ok(self.sandbox.cow_state(id).await)
    }

    async fn cow_state_all(&self) -> Result<Vec<CowStateRecord>, SandboxError> {
        Ok(self.sandbox.cow_state_all().await)
    }

    async fn flush_sandbox(
        &self,
        id: SandboxId,
    ) -> Result<Option<engram_core::types::manifest::ManifestRef>, SandboxError> {
        self.sandbox.flush_sandbox(id).await
    }
}

fn harness_err_to_sandbox(e: HarnessError) -> SandboxError {
    match e {
        HarnessError::NotAttached => SandboxError::NotFound,
        HarnessError::Io(io) => SandboxError::Io(io),
        HarnessError::SessionMismatch { .. } => SandboxError::InvalidSpec(format!("{e}")),
        HarnessError::CheckpointAlreadyInFlight
        | HarnessError::CommandTimeout
        | HarnessError::WriterClosed => SandboxError::Vm(format!("{e}").into()),
    }
}
