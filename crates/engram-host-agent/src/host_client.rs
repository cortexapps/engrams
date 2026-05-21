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
use engram_core::types::egress::SessionEgressPolicy;
use engram_core::types::sandbox::{AgentSpec, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::types::{SandboxId, SessionId};

use crate::harness::{HarnessError, HarnessHub};

#[derive(Clone)]
pub struct LocalHostClient {
    sandbox: Arc<dyn SandboxBackend>,
    harness_hub: Arc<HarnessHub>,
    /// ADR 0014 warm pool. `None` in dev (mode=all) and in tests
    /// that don't exercise warm-lease. When `Some`, the
    /// `lease_warm_sandbox` / `launch_warm_sandbox` /
    /// `list_warm_slots` trait methods delegate here instead of
    /// the no-op defaults.
    warm_pool: Option<crate::warm_pool::WarmPool>,
    /// ADR 0014 M1.12: needed by `launch_warm_sandbox` to resolve
    /// the session's `harness_pack_uri` to a host-local ext4 path
    /// before calling `WarmPool::launch` (which then does the FC
    /// `swap_harness_drive`). `None` in tests / dev modes that
    /// don't exercise the option-D path.
    image_cache: Option<Arc<crate::image_cache::ImageCache>>,
    /// ADR 0014 M1.12: needed for the egress-proxy CA stamping
    /// step inside the harness ext4 (so the in-VM bootstrap can
    /// inject it into the system trust store). `None` skips the
    /// stamp; the harness ext4 is built without the CA.
    egress_ca_pem: Option<String>,
}

impl LocalHostClient {
    pub fn new(sandbox: Arc<dyn SandboxBackend>, harness_hub: Arc<HarnessHub>) -> Self {
        Self {
            sandbox,
            harness_hub,
            warm_pool: None,
            image_cache: None,
            egress_ca_pem: None,
        }
    }

    /// Attach a warm pool (ADR 0014). The host-agent's boot path
    /// constructs one with the same backend instance and calls this
    /// before publishing the LocalHostClient to the gRPC server.
    pub fn with_warm_pool(mut self, warm_pool: crate::warm_pool::WarmPool) -> Self {
        self.warm_pool = Some(warm_pool);
        self
    }

    /// Attach the host's image cache so `launch_warm_sandbox` can
    /// resolve the session's `harness_pack_uri` to a local ext4
    /// path (ADR 0014 M1.12 option D).
    pub fn with_image_cache(
        mut self,
        image_cache: Arc<crate::image_cache::ImageCache>,
        egress_ca_pem: Option<String>,
    ) -> Self {
        self.image_cache = Some(image_cache);
        self.egress_ca_pem = egress_ca_pem;
        self
    }

    /// Borrow the attached warm pool (for the heartbeat loop's
    /// `list_slots` + `observe_templates` hooks in M1.7).
    pub fn warm_pool(&self) -> Option<&crate::warm_pool::WarmPool> {
        self.warm_pool.as_ref()
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
        self.sandbox.destroy(id).await
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        self.sandbox.list().await
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

    async fn commit_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.sandbox.commit_snapshot(id).await
    }

    async fn abort_snapshot(&self, id: SandboxId) -> Result<(), SandboxError> {
        self.sandbox.abort_snapshot(id).await
    }

    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        self.sandbox.restore(metadata).await
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

    async fn guest_ip(&self, id: SandboxId) -> Option<String> {
        self.sandbox.guest_ip(id).await
    }

    async fn bind_session(&self, session_id: SessionId, sandbox_id: SandboxId) {
        self.harness_hub.bind_session(session_id, sandbox_id);
    }

    async fn unbind_session(&self, session_id: SessionId) {
        self.harness_hub.unbind_session(session_id);
    }

    async fn send_prompt(&self, sandbox_id: SandboxId, text: String) -> Result<(), SandboxError> {
        self.harness_hub
            .send_prompt(sandbox_id, text)
            .await
            .map_err(harness_err_to_sandbox)
    }

    async fn acquire_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.harness_hub.acquire_shell(sandbox_id);
        Ok(())
    }

    async fn release_shell(&self, sandbox_id: SandboxId) -> Result<(), SandboxError> {
        self.harness_hub.release_shell(sandbox_id);
        Ok(())
    }

    async fn proxy_shell(
        &self,
        sandbox_id: SandboxId,
    ) -> Result<engram_core::types::shell::ShellTunnel, SandboxError> {
        // ADR 0014 issue #6: dial ttyd in the right netns and bridge
        // WS frames through a ShellTunnel pair. The host's own
        // SandboxBackend knows whether this sandbox is warm-restored
        // (`netns_name_for == Some`) or cold (`None`); the proxy_shell
        // module handles both cases.
        //
        // start_shell() asks the in-VM agentd to ensure ttyd is up
        // and accepting on its port before we attempt the dial. The
        // agentd probes a local TCP connect before replying, so
        // when we get here we're guaranteed a listener exists — no
        // more racing the warm-restore against the snapshot's init
        // script (prod session 73fe33a3 on 2026-05-20 saw the host
        // dial 48s after lease and still hit Connection refused).
        let port = self.sandbox.start_shell(sandbox_id).await?;
        // `vm_internal_ip` not `guest_ip`. `guest_ip` returns the
        // SNAT'd IP for warm-restored sandboxes (used by the
        // egress-proxy registry) — but the shell-tab dial happens
        // INSIDE the per-VM netns, where ttyd is at the VM's
        // in-VM eth0 IP (the bake CIDR's guest octet), NOT the
        // netns's veth IP. Using `guest_ip` here dials the netns's
        // own veth and misses the VM (caught by
        // `crates/engram-host-agent/tests/e2e_shell.rs::e2e_shell_warm`).
        let guest_ip = self
            .sandbox
            .vm_internal_ip(sandbox_id)
            .await
            .ok_or_else(|| SandboxError::Vm("proxy_shell: vm_internal_ip unavailable".into()))?;
        let netns_name = self.sandbox.netns_name_for(sandbox_id).await;
        let (tunnel, ends) = engram_core::types::shell::ShellTunnel::pair();
        crate::proxy_shell::open_shell_tunnel_at(guest_ip, port, netns_name, ends).await?;
        Ok(tunnel)
    }

    async fn lease_warm_sandbox(
        &self,
        template_ref: engram_core::types::ids::TemplateRef,
    ) -> Result<engram_core::traits::host_client::WarmLeaseOutcome, SandboxError> {
        match self.warm_pool.as_ref() {
            Some(pool) => Ok(pool.lease(template_ref).await),
            None => Ok(engram_core::traits::host_client::WarmLeaseOutcome::NoCapacity),
        }
    }

    async fn launch_warm_sandbox(
        &self,
        sandbox_id: SandboxId,
        agent: AgentSpec,
        policy: SessionEgressPolicy,
        harness_pack_uri: Option<String>,
        harness_name: Option<String>,
    ) -> Result<(), SandboxError> {
        match self.warm_pool.as_ref() {
            Some(pool) => {
                // ADR 0014 M1.12: resolve the session's harness URI
                // to a host-local ext4 path so the warm slot's
                // bake-time stub harness drive can be swapped in
                // for it. Sessions with no harness (`kind = none`)
                // pass `None` and skip the swap entirely.
                let session_harness_path = match (harness_pack_uri, self.image_cache.as_ref()) {
                    (Some(uri), Some(cache)) => {
                        // Coord ships the canonical session harness
                        // name explicitly (e.g. "claude"); fall back
                        // to URI-derived naming only for legacy
                        // callers that didn't set the new proto
                        // field. The cold-create path uses the same
                        // canonical name via the
                        // `ENGRAM_SESSION_HARNESS_NAME` env hint, so
                        // both paths agree on the directory layout
                        // bootstrap exec's
                        // (`/run/engram/harnesses/<name>/harness`).
                        let name = harness_name
                            .unwrap_or_else(|| crate::pooled_backend::harness_name_from_uri(&uri));
                        let cached = cache
                            .ensure_harness_ext4(&uri, &name, self.egress_ca_pem.as_deref())
                            .await
                            .map_err(|e| {
                                SandboxError::InvalidSpec(format!(
                                    "warm-lease: harness cache pull {uri}: {e}"
                                ))
                            })?;
                        Some(cached.ext4_path)
                    }
                    (Some(_), None) => {
                        return Err(SandboxError::InvalidSpec(
                            "warm-lease: harness_pack_uri set but no image_cache attached to LocalHostClient".into(),
                        ));
                    }
                    (None, _) => None,
                };
                pool.launch(sandbox_id, agent, policy, session_harness_path)
                    .await
            }
            None => Err(SandboxError::NotFound),
        }
    }

    async fn list_warm_slots(
        &self,
    ) -> Result<Vec<engram_core::traits::host_client::WarmSlotCount>, SandboxError> {
        match self.warm_pool.as_ref() {
            Some(pool) => Ok(pool.list_slots()),
            None => Ok(Vec::new()),
        }
    }

    fn harness_dial(&self) -> HarnessDial {
        self.sandbox.harness_dial()
    }

    fn set_harness_sink(&self, sink: HarnessSink) {
        self.sandbox.set_harness_sink(sink);
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
