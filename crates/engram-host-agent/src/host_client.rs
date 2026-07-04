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
use engram_core::traits::{HarnessDial, HarnessSink, HostClient, SandboxBackend};
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
        let hub = Arc::new(HarnessHub::new(crate::harness::event_sink_to(
            |_, _, _| async {},
        )));
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

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        // Issue #219: drop any lingering shell pin for this sandbox.
        // The pin's release is normally driven by the coord WS bridge,
        // but once the sandbox is gone that bridge can never deliver
        // its `ReleaseShell` — leaving the entry to leak forever. Clear
        // it here (the only host-side layer that both runs on the
        // production destroy path and holds the hub) so the map can't
        // accumulate stale entries over the host's lifetime.
        self.harness_hub.clear_shell(id);
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

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        self.sandbox.snapshot(id).await
    }

    async fn snapshot_begin(
        &self,
        id: SandboxId,
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

    async fn snapshot_wait(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        self.sandbox.snapshot_wait(id).await
    }

    async fn migration_presetup(
        &self,
        id: SandboxId,
    ) -> Result<engram_core::types::snapshot::MigrationPresetupOut, SandboxError> {
        self.sandbox.migration_presetup(id).await
    }

    async fn migration_capture_postcopy(
        &self,
        id: SandboxId,
        export_id: &str,
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

    async fn migration_commit(&self, id: SandboxId, export_id: &str) -> Result<(), SandboxError> {
        self.sandbox.migration_commit(id, export_id).await
    }

    async fn migration_abort(&self, id: SandboxId, export_id: &str) -> Result<(), SandboxError> {
        self.sandbox.migration_abort(id, export_id).await
    }

    async fn commit_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.sandbox.commit_snapshot(id).await
    }

    async fn abort_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.sandbox.abort_snapshot(id).await
    }

    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        self.sandbox.restore(metadata).await
    }

    async fn build_base_snapshot(
        &self,
        spec: SandboxSpec,
        warm: Option<WarmConfig>,
        capture_env: std::collections::HashMap<String, String>,
    ) -> Result<SnapshotMetadata, SandboxError> {
        self.sandbox
            .build_base_snapshot(spec, warm, capture_env)
            .await
    }

    async fn restore_base_for_session(
        &self,
        metadata: SnapshotMetadata,
        session_env: std::collections::HashMap<String, String>,
        selected_mounts: Vec<engram_core::types::sandbox::AuxRoDrive>,
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
    ) -> Result<(), SandboxError> {
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

    async fn bind_session(&self, session_id: SessionId, sandbox_id: SandboxId) {
        self.harness_hub.bind_session(session_id, sandbox_id);
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

    async fn rehandshake(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.harness_hub
            .rehandshake(sandbox_id)
            .await
            .map_err(harness_err_to_sandbox)
    }

    // ADR 0045 Phase F: freeze/unfreeze the microVM in place — pure
    // delegation to the inner backend (no harness involvement).
    async fn pause(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.sandbox.pause(sandbox_id).await
    }

    async fn resume(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.sandbox.resume(sandbox_id).await
    }

    async fn acquire_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.harness_hub.acquire_shell(sandbox_id);
        Ok(())
    }

    async fn release_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.harness_hub.release_shell(sandbox_id);
        Ok(())
    }

    async fn renew_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.harness_hub.renew_shell(sandbox_id);
        Ok(())
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
